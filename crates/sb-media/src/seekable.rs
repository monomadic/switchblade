//! In-process libav playback with a real seek channel (DESIGN.md §15
//! "Low-latency seek", settled 2026-07; spike numbers in `spikes/seek-bench`).
//!
//! `SeekablePlayer` is the sole live player — every lane (selected, warm,
//! hover) rides it. It replaced an earlier CLI-pipe player (an ffmpeg child
//! decoding raw RGBA to stdout): same paced bounded queue, same backpressure
//! warmth, same due-frame contract — but the demuxer + VideoToolbox session
//! live in a reader thread instead of behind an ffmpeg pipe, so `seek()` is a
//! demuxer jump + decoder flush (keyframe: ~10–30ms; exact: bounded by GOP
//! decode, worst ~600ms on long-GOP 4K) instead of a full process respawn
//! (~1s floor: exec + probe + VT session init + GOP, benchmarked unfixable
//! via flags) — which is why the CLI player couldn't seek at all.
//!
//! Decode/scale parity with the CLI chain is deliberate, gate for gate:
//! VideoToolbox decode only for h264/hevc/prores (`vt_accel`); on-GPU
//! scaling via the same `hw_scale_vf` filter string when the cached meta
//! allows it (dims aligned down to mod-8, ±90° via transpose_vt, sw
//! fallback otherwise) — libavfilter parses the exact `-vf` syntax, so the
//! PSNR-verified chain is reused verbatim. The software chain scales
//! `fast_bilinear` and applies rotation itself (libavfilter does not
//! autorotate like the CLI; 90/180/270 are handled, odd angles are not —
//! same clips the hw chain already refuses).
//!
//! Pacing stamps frames due by *pts delta* from a wall-clock anchor (the old
//! CLI player forced CFR with `-r` and stamped 1/fps; pts-based stamping
//! degrades VFR/wrong-meta sources the same way — correct wall-clock speed).
//! Late frames re-anchor instead of accruing debt, and a drop must wake a
//! reader parked on the full-queue condvar — both regressions carried over
//! from that CLI player, both covered by tests below.

use rsmpeg::ffi;
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{LIVE_QUEUE_DEPTH, Meta};

/// A decode hiccup shorter than this rides the queue's read-ahead; anything
/// later re-anchors the schedule (owed frames would otherwise all come due
/// at once and play as a fast-forward burst).
const LATE_SLACK: Duration = Duration::from_millis(50);

pub struct SeekablePlayer {
    shared: Arc<Shared>,
    pub w: u32,
    pub h: u32,
}

struct Shared {
    /// (due, pts seconds, rgba) — the paced, bounded live decode queue.
    frames: Mutex<VecDeque<(Instant, f64, Vec<u8>)>>,
    /// Signalled when the consumer pops (or a seek/drop needs the reader).
    space: Condvar,
    /// Raised on drop: a stalled player's reader parks on `space` with a
    /// full queue (every warm player's steady state) and nothing else can
    /// reach it there — without this wake the thread leaks, pinning its
    /// frame buffers (the CLI player's ~30MB-per-drop bug, same shape).
    closed: AtomicBool,
    /// Latest seek request (seconds, exact) — newest wins, the reader
    /// takes it between packets and inside the full-queue wait.
    cmd: Mutex<Option<(f64, bool)>>,
    /// f64 bits: pts of the frame most recently *taken* (on screen), or
    /// the seek target while one is in flight — "where playback is".
    position: AtomicU64,
    failed: AtomicBool,
    /// Fired when a frame lands in a previously EMPTY queue (P1.4): the
    /// render loop sleeps toward `next_due()` instead of spinning at
    /// display rate, so a frame arriving while the queue was dry needs
    /// its own nudge or it would wait out the idle tick.
    notify: Mutex<Option<crate::Notify>>,
    /// Recycled frame buffers (P1.5): a hires frame is ~33MB at 4K, and
    /// allocating/freeing one per decoded frame taxed both the reader
    /// (alloc) and the render thread (free after upload) with mmap-sized
    /// allocator churn. Superseded/seek-stale frames and presented
    /// buffers handed back via `recycle()` land here; the reader reuses
    /// them for the next copy. Lock order: may be taken while `frames`
    /// is held, never the reverse.
    pool: Mutex<Vec<Vec<u8>>>,
    /// Benchmark instrumentation, attached after spawn (bench runs only,
    /// `None` otherwise). The reader thread tags its media-side events —
    /// first-frame-ready and pacing re-anchors — with this lane's
    /// identity; see `probe.rs`. Locked only at those (rare) branches.
    probe: Mutex<Option<crate::LaneProbe>>,
}

/// Recycled buffers kept per player: queue depth + one being copied +
/// one at the display, with a little slack. Excess just drops.
const POOL_CAP: usize = LIVE_QUEUE_DEPTH + 2;

impl Shared {
    /// A buffer sized `len`, recycled when the pool has one. Same-size
    /// reuse (the steady state — one player, one frame size) touches no
    /// allocator at all.
    fn take_buf(&self, len: usize) -> Vec<u8> {
        let mut buf = self.pool.lock().unwrap().pop().unwrap_or_default();
        buf.resize(len, 0);
        buf
    }

    fn recycle_buf(&self, buf: Vec<u8>) {
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < POOL_CAP {
            pool.push(buf);
        }
    }
}

impl SeekablePlayer {
    /// `seek` starts that many (content-relative) seconds in — a keyframe
    /// start matching the CLI thumbnail's `-ss … -noaccurate_seek`, so live
    /// video continues from the exact frame the tile showed. `meta` gates
    /// hardware decode and scaling; hardware-scaled dims round DOWN to mod-8
    /// exactly like the ffmpeg thumbnail chain. Returns immediately; open/
    /// decode errors surface as a stream that never produces frames
    /// (`failed()`).
    pub fn spawn(path: &Path, w: u32, h: u32, seek: f64, meta: Option<&Meta>) -> Option<Self> {
        let (mut w, mut h) = (w.max(2), h.max(2));
        let codec = meta.and_then(|m| m.codec.as_deref());
        let rotation = meta.and_then(|m| m.rotation);
        let hw_chain = if w >= 8 && h >= 8 {
            crate::hw_scale_vf(
                codec,
                meta.and_then(|m| m.pix_fmt.as_deref()),
                rotation,
                w & !7,
                h & !7,
            )
        } else {
            None
        };
        if hw_chain.is_some() {
            (w, h) = (w & !7, h & !7);
        }
        // The software chain (also the fallback when the hw plan doesn't
        // survive contact with the actual stream). Rotation is explicit:
        // the mapping mirrors the PSNR-verified transpose_vt directions.
        let sw_pre = match rotation.map(|r| {
            let q = (r / 90.0).round();
            if (r - q * 90.0).abs() > 1.0 {
                -1
            } else {
                (q as i64).rem_euclid(4)
            }
        }) {
            Some(1) => "transpose=2,",
            Some(3) => "transpose=1,",
            Some(2) => "hflip,vflip,",
            _ => "",
        };
        let sw_chain = format!("{sw_pre}scale={w}:{h}:flags=fast_bilinear,format=rgba");
        let cfg = ReaderCfg {
            path: path.to_path_buf(),
            cpath: c_path(path)?,
            w,
            h,
            start: seek.max(0.0),
            use_vt: crate::vt_accel(codec),
            hw_chain: hw_chain.and_then(|s| CString::new(s).ok()),
            sw_chain: CString::new(sw_chain).ok()?,
        };
        let shared = Arc::new(Shared {
            frames: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
            closed: AtomicBool::new(false),
            cmd: Mutex::new(None),
            position: AtomicU64::new(seek.max(0.0).to_bits()),
            failed: AtomicBool::new(false),
            notify: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
            probe: Mutex::new(None),
        });
        let reader_shared = shared.clone();
        thread::spawn(move || {
            // All libav state is created, used and freed on this thread.
            if let Err(e) = unsafe { reader(&reader_shared, &cfg) } {
                log::warn!("seekable player: {} — {e}", cfg.path.display());
                reader_shared.failed.store(true, Ordering::Relaxed);
            }
        });
        Some(Self { shared, w, h })
    }

    /// Jump playback. `exact` decodes forward from the preceding keyframe
    /// to the true target (GOP-bound); otherwise the landing keyframe
    /// plays immediately. Queued frames are stale the moment this is
    /// called, so they're dropped — the last shown frame stays on screen
    /// (the hires texture still holds it) until the new position delivers.
    pub fn seek(&self, target_s: f64, exact: bool) {
        let t = target_s.max(0.0);
        *self.shared.cmd.lock().unwrap() = Some((t, exact));
        // Stale frames go back to the pool, not the allocator (P1.5): a
        // scrub used to free the whole queue — up to ~100MB at 4K — on
        // the caller's (render) thread.
        for (_, _, buf) in self.shared.frames.lock().unwrap().drain(..) {
            self.shared.recycle_buf(buf);
        }
        // Report the destination as the position while in flight: the bar
        // should show where playback is going, not the frozen frame.
        self.shared.position.store(t.to_bits(), Ordering::Relaxed);
        self.shared.space.notify_all();
    }

    /// Seconds into the clip: the pts of the frame currently on screen,
    /// or the seek target while one is in flight.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.shared.position.load(Ordering::Relaxed))
    }

    /// The reader hit an unrecoverable open/decode error; no frames will
    /// ever arrive (callers already tolerate frameless streams).
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// Frames currently queued (decoded, waiting for their due times).
    pub fn buffered(&self) -> usize {
        self.shared.frames.lock().unwrap().len()
    }

    /// When the next queued frame wants presenting — the render loop's
    /// wake deadline while nothing else animates (P1.4). None while the
    /// queue is dry (the reader's push-notify covers that gap).
    pub fn next_due(&self) -> Option<Instant> {
        self.shared
            .frames
            .lock()
            .unwrap()
            .front()
            .map(|(due, ..)| *due)
    }

    /// Install the wake fired when a frame lands in an empty queue.
    pub fn set_notify(&self, f: crate::Notify) {
        *self.shared.notify.lock().unwrap() = Some(f);
    }

    /// Attach benchmark instrumentation so this lane's reader thread emits
    /// identity-tagged media events (first-frame-ready, re-anchors) and
    /// bumps the shared counters. Bench runs only; normal runs never call
    /// it, so the reader's probe branches stay `None`.
    pub fn attach_probe(&self, lp: crate::LaneProbe) {
        *self.shared.probe.lock().unwrap() = Some(lp);
    }

    /// The newest frame that's due for presentation, if any — pacing on
    /// the render clock (a frame surfaces only once its due time arrives).
    pub fn take_frame(&self) -> Option<Vec<u8>> {
        let now = Instant::now();
        let mut q = self.shared.frames.lock().unwrap();
        let mut out = None;
        while q.front().is_some_and(|(due, _, _)| *due <= now) {
            let (_, pts, rgba) = q.pop_front().unwrap();
            self.shared.position.store(pts.to_bits(), Ordering::Relaxed);
            // Catch-up after a hiccup: superseded frames recycle instead
            // of freeing several large buffers in one render frame (P1.5).
            if let Some(prev) = out.replace(rgba) {
                self.shared.recycle_buf(prev);
            }
        }
        if out.is_some() {
            self.shared.space.notify_one();
        }
        out
    }

    /// Hand a presented frame's buffer back for reuse (P1.5). Callers
    /// that upload and drop instead lose nothing but the recycling.
    pub fn recycle(&self, buf: Vec<u8>) {
        self.shared.recycle_buf(buf);
    }
}

impl Drop for SeekablePlayer {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.space.notify_all();
    }
}

struct ReaderCfg {
    path: PathBuf,
    cpath: CString,
    w: u32,
    h: u32,
    start: f64,
    use_vt: bool,
    /// The CLI `-vf` string from `hw_scale_vf`, reused verbatim as a
    /// libavfilter graph (same syntax) when the first frame really is a
    /// VideoToolbox surface.
    hw_chain: Option<CString>,
    sw_chain: CString,
}

#[cfg(unix)]
fn c_path(p: &Path) -> Option<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).ok()
}
#[cfg(not(unix))]
fn c_path(p: &Path) -> Option<CString> {
    CString::new(p.to_string_lossy().as_bytes()).ok()
}

/// Why the reader stopped pushing a frame.
enum Flow {
    Continue,
    /// Player dropped — unwind and free everything.
    Stop,
}

unsafe extern "C" fn get_hw_format(
    _ctx: *mut ffi::AVCodecContext,
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        let mut p = fmts;
        while *p != ffi::AV_PIX_FMT_NONE {
            if *p == ffi::AV_PIX_FMT_VIDEOTOOLBOX {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

// RAII for the libav objects the reader owns, so every early return frees.
struct FmtCtx(*mut ffi::AVFormatContext);
impl Drop for FmtCtx {
    fn drop(&mut self) {
        unsafe { ffi::avformat_close_input(&mut self.0) }
    }
}
struct DecCtx(*mut ffi::AVCodecContext);
impl Drop for DecCtx {
    fn drop(&mut self) {
        unsafe { ffi::avcodec_free_context(&mut self.0) }
    }
}
struct HwDev(*mut ffi::AVBufferRef);
impl Drop for HwDev {
    fn drop(&mut self) {
        unsafe { ffi::av_buffer_unref(&mut self.0) }
    }
}
struct FramePtr(*mut ffi::AVFrame);
impl Drop for FramePtr {
    fn drop(&mut self) {
        unsafe { ffi::av_frame_free(&mut self.0) }
    }
}
struct PktPtr(*mut ffi::AVPacket);
impl Drop for PktPtr {
    fn drop(&mut self) {
        unsafe { ffi::av_packet_free(&mut self.0) }
    }
}
struct Graph {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    /// pts unit of frames coming off the sink.
    tb: f64,
}
impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { ffi::avfilter_graph_free(&mut self.graph) }
    }
}

/// Everything the per-frame path needs, bundled so `handle_frame` stays a
/// function instead of a closure over a dozen locals.
struct Pump<'a> {
    shared: &'a Shared,
    cfg: &'a ReaderCfg,
    /// Stream timebase: seconds per pts unit (and the rational itself,
    /// for declaring the buffersrc input — frames carry pts in it).
    tb: f64,
    tb_q: ffi::AVRational,
    /// Absolute stream start_time in seconds (edit-list / start_time
    /// offset). Subtracted from every frame pts so the queue, `position`,
    /// and `skip_until` all speak content-relative time — see `reader`.
    start_off: f64,
    graph: Option<Graph>,
    /// Destination of `av_hwframe_transfer_data` when frames are hw but
    /// no hw filter chain applies (parity with plain `-hwaccel` CLI mode:
    /// download at native res, scale in software).
    transfer: FramePtr,
    filtered: FramePtr,
    /// Exact-seek refinement: drop decoded frames until this pts.
    skip_until: Option<f64>,
    /// (wall, pts) pacing anchor; None re-anchors on the next frame.
    anchor: Option<(Instant, f64)>,
    /// Bench probe: emit `DecodeReady` on the first queued frame only.
    first_ready: bool,
}

/// libav's default log callback prints straight to the process's stderr,
/// and this in-process reader is the only lane that runs it — so its
/// chatter bypasses the `Stdio::null()` the CLI lanes use. The loudest of
/// it is the benign swscale note "No accelerated colorspace conversion
/// found from yuv420p to rgba": the software `format=rgba` chain has no
/// SIMD path for that pair, so swscale falls back to a C converter and
/// says so once per slice-thread context. It is not an error; the reader
/// already surfaces real failures via `failed` + `log::warn`. Clamp libav
/// to ERROR by default, but honor `RUST_LOG=debug` for decode diagnosis.
fn quiet_libav_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let level = if log::log_enabled!(log::Level::Debug) {
            ffi::AV_LOG_WARNING
        } else {
            ffi::AV_LOG_ERROR
        };
        unsafe { ffi::av_log_set_level(level as i32) };
    });
}

unsafe fn reader(shared: &Shared, cfg: &ReaderCfg) -> Result<(), String> {
    quiet_libav_once();
    unsafe {
        let mut fmt_raw: *mut ffi::AVFormatContext = ptr::null_mut();
        if ffi::avformat_open_input(
            &mut fmt_raw,
            cfg.cpath.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
        ) < 0
        {
            return Err("open failed".into());
        }
        let fmt = FmtCtx(fmt_raw);
        if ffi::avformat_find_stream_info(fmt.0, ptr::null_mut()) < 0 {
            return Err("no stream info".into());
        }
        let mut codec: *const ffi::AVCodec = ptr::null();
        let sidx = ffi::av_find_best_stream(fmt.0, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, &mut codec, 0);
        if sidx < 0 {
            return Err("no video stream".into());
        }
        let stream = *(*fmt.0).streams.add(sidx as usize);
        let stream_tb = (*stream).time_base;
        let tb = stream_tb.num as f64 / stream_tb.den as f64;
        // Content-relative time offset. `avformat_seek_file` and frame pts
        // are in ABSOLUTE stream timestamps, but sb-app (and the CLI `-ss`
        // thumbnail seek) work content-relative — 0 = the first frame. On
        // files with a non-zero start_time / edit list (ubiquitous in
        // phone/camera/QuickTime footage) the two disagree by `start_time`:
        // a content-relative seek target hit the wrong absolute keyframe,
        // so the live stream opened on a DIFFERENT frame than the static
        // thumb (the broken no-jolt handoff the user hit). Add this when
        // seeking, subtract it when stamping position, so the whole player
        // speaks content-relative time and matches the thumb keyframe.
        let start_off = {
            let st = (*stream).start_time;
            if st == ffi::AV_NOPTS_VALUE {
                0.0
            } else {
                st as f64 * tb
            }
        };

        let dec = DecCtx(ffi::avcodec_alloc_context3(codec));
        if ffi::avcodec_parameters_to_context(dec.0, (*stream).codecpar) < 0 {
            return Err("codec params failed".into());
        }
        (*dec.0).thread_count = 0; // auto, like the CLI
        // Device creation failing is fine: frames arrive software and
        // take the sw chain, exactly like CLI hwaccel fallback.
        let mut _hw_dev = HwDev(ptr::null_mut());
        if cfg.use_vt
            && ffi::av_hwdevice_ctx_create(
                &mut _hw_dev.0,
                ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                ptr::null(),
                ptr::null_mut(),
                0,
            ) == 0
        {
            (*dec.0).hw_device_ctx = ffi::av_buffer_ref(_hw_dev.0);
            (*dec.0).get_format = Some(get_hw_format);
        }
        if ffi::avcodec_open2(dec.0, codec, ptr::null_mut()) < 0 {
            return Err("decoder open failed".into());
        }

        let pkt = PktPtr(ffi::av_packet_alloc());
        let frame = FramePtr(ffi::av_frame_alloc());
        let mut pump = Pump {
            shared,
            cfg,
            tb,
            tb_q: stream_tb,
            start_off,
            graph: None,
            transfer: FramePtr(ffi::av_frame_alloc()),
            filtered: FramePtr(ffi::av_frame_alloc()),
            skip_until: None,
            anchor: None,
            first_ready: true,
        };

        let seek_to = |target: f64| -> bool {
            // `target` is content-relative; seek in absolute stream time.
            let ts = ((target + start_off) / tb) as i64;
            let ok = ffi::avformat_seek_file(
                fmt.0,
                sidx,
                i64::MIN,
                ts,
                ts,
                ffi::AVSEEK_FLAG_BACKWARD as i32,
            ) >= 0;
            ffi::avcodec_flush_buffers(dec.0);
            ok
        };
        if cfg.start > 0.05 {
            // Keyframe start, matching the thumbnail's keyframe grab
            // (`-noaccurate_seek` in extract_frame): land on the keyframe
            // ≤ start and play from it — no decode-forward. The static
            // thumb and this first live frame share that keyframe, so the
            // handoff stays jolt-free (skip_until = None: don't discard the
            // pre-target GOP frames, the keyframe IS the target now).
            seek_to(cfg.start);
        }

        loop {
            if shared.closed.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some((target, exact)) = shared.cmd.lock().unwrap().take() {
                seek_to(target);
                pump.skip_until = exact.then_some(target);
                pump.anchor = None;
                continue;
            }
            let r = ffi::av_read_frame(fmt.0, pkt.0);
            if r == ffi::AVERROR_EOF {
                // Drain the decoder's tail, then loop the clip (the CLI
                // used `-stream_loop -1`; a resident demuxer just seeks).
                let _ = ffi::avcodec_send_packet(dec.0, ptr::null());
                loop {
                    let rr = ffi::avcodec_receive_frame(dec.0, frame.0);
                    if rr < 0 {
                        break;
                    }
                    if let Flow::Stop = pump.frame_in(frame.0) {
                        return Ok(());
                    }
                }
                seek_to(0.0);
                pump.skip_until = None;
                pump.anchor = None;
                continue;
            }
            if r < 0 {
                return Err(format!("read error {r}"));
            }
            if (*pkt.0).stream_index != sidx {
                ffi::av_packet_unref(pkt.0);
                continue;
            }
            let sr = ffi::avcodec_send_packet(dec.0, pkt.0);
            ffi::av_packet_unref(pkt.0);
            if sr < 0 && sr != ffi::AVERROR(ffi::EAGAIN) {
                // Corrupt packet: skip it, keep the stream alive.
                continue;
            }
            loop {
                let rr = ffi::avcodec_receive_frame(dec.0, frame.0);
                if rr == ffi::AVERROR(ffi::EAGAIN) || rr == ffi::AVERROR_EOF {
                    break;
                }
                if rr < 0 {
                    return Err("decode error".into());
                }
                if let Flow::Stop = pump.frame_in(frame.0) {
                    return Ok(());
                }
            }
        }
    }
}

impl Pump<'_> {
    /// One decoded frame in: exact-seek discard, hw download when the hw
    /// chain doesn't apply, lazy graph build, filter, pace, push.
    unsafe fn frame_in(&mut self, frame: *mut ffi::AVFrame) -> Flow {
        unsafe {
            let pts = (*frame).best_effort_timestamp;
            // Content-relative: pts are absolute stream timestamps; the
            // whole player (queue due-stamps, position, skip_until) speaks
            // content time so it matches sb-app and the CLI thumb seek.
            let pts_s = pts as f64 * self.tb - self.start_off;
            if let Some(t) = self.skip_until {
                if pts != ffi::AV_NOPTS_VALUE && pts_s < t - 1e-3 {
                    ffi::av_frame_unref(frame);
                    return Flow::Continue; // pre-target GOP frame
                }
                self.skip_until = None;
            }
            let is_hw = (*frame).format == ffi::AV_PIX_FMT_VIDEOTOOLBOX;
            let use_hw_chain = is_hw && self.cfg.hw_chain.is_some();
            let feed = if is_hw && !use_hw_chain {
                // Plain-hwaccel parity: download at native res, sw scale.
                ffi::av_frame_unref(self.transfer.0);
                if ffi::av_hwframe_transfer_data(self.transfer.0, frame, 0) < 0 {
                    ffi::av_frame_unref(frame);
                    return Flow::Continue;
                }
                let _ = ffi::av_frame_copy_props(self.transfer.0, frame);
                ffi::av_frame_unref(frame);
                self.transfer.0
            } else {
                frame
            };
            if self.graph.is_none() {
                let chain = if use_hw_chain {
                    self.cfg.hw_chain.as_ref().unwrap()
                } else {
                    &self.cfg.sw_chain
                };
                match build_graph(feed, chain, self.tb_q) {
                    Ok(g) => {
                        log::debug!(
                            "seekable graph ({}): {}",
                            if use_hw_chain { "hw scale" } else { "sw scale" },
                            self.cfg.path.display()
                        );
                        self.graph = Some(g);
                    }
                    Err(e) => {
                        log::warn!(
                            "seekable player: filter graph failed ({e}): {}",
                            self.cfg.path.display()
                        );
                        ffi::av_frame_unref(feed);
                        self.shared.failed.store(true, Ordering::Relaxed);
                        return Flow::Stop;
                    }
                }
            }
            let (g_src, g_sink, g_tb) = {
                let g = self.graph.as_ref().unwrap();
                (g.src, g.sink, g.tb)
            };
            if ffi::av_buffersrc_add_frame(g_src, feed) < 0 {
                ffi::av_frame_unref(feed);
                return Flow::Continue;
            }
            loop {
                let r = ffi::av_buffersink_get_frame(g_sink, self.filtered.0);
                if r < 0 {
                    return Flow::Continue; // EAGAIN/EOF: need more input
                }
                // Content-relative (subtract start_time), so the queue's
                // due-stamps and `position` match sb-app / the thumb seek.
                let out_pts = (*self.filtered.0).pts as f64 * g_tb - self.start_off;
                let flow = self.push_rgba(out_pts);
                ffi::av_frame_unref(self.filtered.0);
                if let Flow::Stop = flow {
                    return Flow::Stop;
                }
            }
        }
    }

    /// Copy the filtered rgba frame out and queue it with its due time.
    unsafe fn push_rgba(&mut self, pts_s: f64) -> Flow {
        unsafe {
            let f = self.filtered.0;
            let (w, h) = (self.cfg.w as usize, self.cfg.h as usize);
            if (*f).format != ffi::AV_PIX_FMT_RGBA
                || (*f).width as usize != w
                || (*f).height as usize != h
            {
                return Flow::Continue; // negotiation surprise: drop, don't crash
            }
            // Bounded read-ahead: park until the consumer makes room — or
            // the player is dropped, or a seek makes this frame stale.
            // This wait runs BEFORE the RGBA allocation/copy (P1.3): a
            // parked warm lane then retains only its queued frames, never
            // a fourth pre-copied one (~14 MiB per lane at 1440p), and a
            // seek that lands while parked skips the copy entirely. The
            // lock drops for the copy itself — room only ever grows (one
            // reader per player), so the re-check after is just for
            // close/seek races.
            {
                let mut q = self.shared.frames.lock().unwrap();
                loop {
                    if self.shared.closed.load(Ordering::Relaxed) {
                        return Flow::Stop;
                    }
                    if self.shared.cmd.lock().unwrap().is_some() {
                        return Flow::Continue; // stale: the seek handler owns what's next
                    }
                    if q.len() < LIVE_QUEUE_DEPTH {
                        break;
                    }
                    q = self.shared.space.wait(q).unwrap();
                }
            }
            // Due time AFTER the park: a long-parked frame re-anchors to
            // the wall clock (late-frame rule) instead of carrying a
            // deadline that went stale while it waited.
            let now = Instant::now();
            let due = match self.anchor {
                // Monotonic pts ahead of the anchor and not badly late:
                // schedule by pts delta (frame-rate independent pacing).
                Some((w0, p0)) if pts_s >= p0 => {
                    let d = w0 + Duration::from_secs_f64(pts_s - p0);
                    if d + LATE_SLACK < now {
                        self.anchor = Some((now, pts_s)); // late: re-anchor
                        if let Some(lp) = &*self.shared.probe.lock().unwrap() {
                            lp.sink.counters.late_frames.fetch_add(1, Ordering::Relaxed);
                            lp.sink.counters.reanchors.fetch_add(1, Ordering::Relaxed);
                            lp.mark(now, crate::EventKind::Reanchor);
                        }
                        now
                    } else {
                        d
                    }
                }
                // First frame, post-seek, or a pts jump backward (loop
                // restart): re-anchor to the wall clock.
                _ => {
                    self.anchor = Some((now, pts_s));
                    now
                }
            };
            let stride = (*f).linesize[0] as usize;
            let row = w * 4;
            // Recycled when the pool has a buffer (P1.5) — the copy below
            // overwrites every byte, so stale contents don't matter.
            let mut buf = self.shared.take_buf(row * h);
            let src = (*f).data[0];
            if stride == row {
                ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), row * h);
            } else {
                for y in 0..h {
                    ptr::copy_nonoverlapping(src.add(y * stride), buf[y * row..].as_mut_ptr(), row);
                }
            }
            let mut q = self.shared.frames.lock().unwrap();
            if self.shared.closed.load(Ordering::Relaxed) {
                return Flow::Stop;
            }
            if self.shared.cmd.lock().unwrap().is_some() {
                return Flow::Continue; // seek landed during the copy: stale
            }
            let was_dry = q.is_empty();
            q.push_back((due, pts_s, buf));
            drop(q);
            // First frame the decoder ever queued: the media end of the
            // spawn→ready latency (bench probe, no-op otherwise).
            if self.first_ready {
                self.first_ready = false;
                if let Some(lp) = &*self.shared.probe.lock().unwrap() {
                    lp.mark(now, crate::EventKind::DecodeReady);
                }
            }
            // A deadline-sleeping render loop (P1.4) has no due time to
            // wait for while the queue is dry — this push is its wake.
            if was_dry && let Some(f) = &*self.shared.notify.lock().unwrap() {
                f();
            }
            Flow::Continue
        }
    }
}

/// buffersrc → parsed chain → buffersink. Built from the first frame's
/// actual properties (format, dims, and the hw frames ctx when present),
/// which is what lets the CLI `-vf` strings work unchanged.
unsafe fn build_graph(
    frame: *const ffi::AVFrame,
    chain: &CString,
    tb: ffi::AVRational,
) -> Result<Graph, String> {
    unsafe {
        let mut g = Graph {
            graph: ffi::avfilter_graph_alloc(),
            src: ptr::null_mut(),
            sink: ptr::null_mut(),
            tb: 0.0,
        };
        if g.graph.is_null() {
            return Err("graph alloc".into());
        }
        let src_def = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        let sink_def = ffi::avfilter_get_by_name(c"buffersink".as_ptr());
        g.src = ffi::avfilter_graph_alloc_filter(g.graph, src_def, c"in".as_ptr());
        if g.src.is_null() {
            return Err("buffersrc alloc".into());
        }
        let par = ffi::av_buffersrc_parameters_alloc();
        (*par).format = (*frame).format;
        (*par).width = (*frame).width;
        (*par).height = (*frame).height;
        // Colorspace/range too: buffersrc defaults them to unspecified,
        // and real camera output tags bt709/tv — the mismatch makes every
        // spawn log "Changing video frame properties on the fly".
        (*par).color_space = (*frame).colorspace;
        (*par).color_range = (*frame).color_range;
        // Declare the true stream timebase: frames carry pts in it, and
        // the sink's own timebase (read back below) prices the output.
        (*par).time_base = tb;
        if !(*frame).hw_frames_ctx.is_null() {
            (*par).hw_frames_ctx = (*frame).hw_frames_ctx;
        }
        let pr = ffi::av_buffersrc_parameters_set(g.src, par);
        ffi::av_free(par as *mut _);
        if pr < 0 {
            return Err("buffersrc params".into());
        }
        if ffi::avfilter_init_str(g.src, ptr::null()) < 0 {
            return Err("buffersrc init".into());
        }
        if ffi::avfilter_graph_create_filter(
            &mut g.sink,
            sink_def,
            c"out".as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            g.graph,
        ) < 0
        {
            return Err("buffersink".into());
        }
        // Wire "[in] chain [out]" between our endpoints (filtering_video.c
        // pattern: the labels name OUR filters' open pads).
        let outputs = ffi::avfilter_inout_alloc();
        let inputs = ffi::avfilter_inout_alloc();
        (*outputs).name = ffi::av_strdup(c"in".as_ptr());
        (*outputs).filter_ctx = g.src;
        (*outputs).pad_idx = 0;
        (*outputs).next = ptr::null_mut();
        (*inputs).name = ffi::av_strdup(c"out".as_ptr());
        (*inputs).filter_ctx = g.sink;
        (*inputs).pad_idx = 0;
        (*inputs).next = ptr::null_mut();
        let mut inputs = inputs;
        let mut outputs = outputs;
        let pr = ffi::avfilter_graph_parse_ptr(
            g.graph,
            chain.as_ptr(),
            &mut inputs,
            &mut outputs,
            ptr::null_mut(),
        );
        ffi::avfilter_inout_free(&mut inputs);
        ffi::avfilter_inout_free(&mut outputs);
        if pr < 0 {
            return Err("graph parse".into());
        }
        if ffi::avfilter_graph_config(g.graph, ptr::null_mut()) < 0 {
            return Err("graph config".into());
        }
        let sink_tb = ffi::av_buffersink_get_time_base(g.sink);
        g.tb = sink_tb.num as f64 / sink_tb.den as f64;
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Generate (once) a small h264 test clip for the pacing/seek tests.
    /// Returns None when ffmpeg isn't on PATH (tests skip quietly).
    fn test_clip(name: &str, secs: u32) -> Option<PathBuf> {
        if !crate::have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return None;
        }
        let dir = std::env::temp_dir().join("sb_media_seekable_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join(name);
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg(format!("testsrc2=duration={secs}:size=320x180:rate=30"))
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        Some(clip)
    }

    /// h264 yuv420p @30 — on macOS this takes the hardware scale chain,
    /// so the tests cover the scale_vt graph path.
    fn meta(clip: &Path) -> Meta {
        Meta {
            src: clip.to_path_buf(),
            duration: None,
            width: None,
            height: None,
            codec: Some("h264".into()),
            fps: Some(30.0),
            rotation: None,
            pix_fmt: Some("yuv420p".into()),
        }
    }

    fn take_one(p: &SeekablePlayer, within: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(f) = p.take_frame() {
                return Some(f);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    /// Same contract as `live_player_paces_frames`: ~rate-paced delivery
    /// from the start — no initial burst, no stall.
    #[test]
    fn seekable_player_paces_frames() {
        let Some(clip) = test_clip("pace.mp4", 4) else {
            return;
        };
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&meta(&clip))).expect("spawn");
        assert!(
            take_one(&p, Duration::from_secs(3)).is_some(),
            "no first frame within 3s"
        );
        let t0 = Instant::now();
        let mut frames = 0u32;
        while t0.elapsed() < Duration::from_secs(1) {
            if p.take_frame().is_some() {
                frames += 1;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            (20..=45).contains(&frames),
            "expected ~30 paced frames in 1s, got {frames}"
        );
        assert!(!p.failed(), "reader reported failure");
    }

    /// An attached probe records the media-side `DecodeReady` event from
    /// the reader thread, tagged with this lane's identity — the proof
    /// that media events reach the shared sink (phase-0-contracts §0.1).
    #[test]
    fn attached_probe_records_decode_ready_from_the_reader_thread() {
        use crate::probe::{Lane, LaneProbe, Probe};
        let Some(clip) = test_clip("probe.mp4", 4) else {
            return;
        };
        let sink = Probe::new();
        sink.record_events();
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&meta(&clip))).expect("spawn");
        p.attach_probe(LaneProbe {
            sink: sink.clone(),
            lane: Lane::Selected,
            generation: 7,
            clip: Arc::from(clip.to_string_lossy().as_ref()),
        });
        let anchor = Instant::now();
        assert!(
            take_one(&p, Duration::from_secs(3)).is_some(),
            "no first frame within 3s"
        );
        // Give the reader a beat to push+emit before draining.
        thread::sleep(Duration::from_millis(20));
        let (evs, _) = sink.drain(anchor);
        let ready = evs.iter().find(|e| e.kind == "decode_ready");
        let ready = ready.expect("a decode_ready event was recorded");
        assert_eq!(ready.lane, "selected");
        assert_eq!(ready.lane_gen, 7);
        assert!(ready.clip.as_deref().unwrap().ends_with("probe.mp4"));
    }

    /// The pre-warm contract (the warm pool rides SeekablePlayer now): an
    /// undrained player fills its bounded queue, stalls at near-zero cost,
    /// and serves a frame the instant it's promoted.
    #[test]
    fn unwatched_seekable_player_stalls_then_serves_instantly() {
        let Some(clip) = test_clip("warm.mp4", 4) else {
            return;
        };
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&meta(&clip))).expect("spawn");
        // Bounded wait, not a fixed sleep — the fixed 800ms passed
        // serially but flaked under parallel-suite contention (T1).
        assert!(
            wait_buffered(&p, LIVE_QUEUE_DEPTH, Duration::from_secs(15)),
            "queue never filled while unwatched"
        );
        assert!(
            p.shared.frames.lock().unwrap().len() <= LIVE_QUEUE_DEPTH,
            "queue must stay bounded while unwatched"
        );
        assert!(
            p.take_frame().is_some(),
            "a warmed player must serve a frame on the first take"
        );
    }

    /// P1.5: a buffer handed back via `recycle()` is reused for a later
    /// frame instead of a fresh allocation — steady playback cycles the
    /// same few buffers with the allocator untouched.
    #[test]
    fn recycled_buffers_are_reused() {
        let Some(clip) = test_clip("recycle.mp4", 4) else {
            return;
        };
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&meta(&clip))).expect("spawn");
        let first = take_one(&p, Duration::from_secs(5)).expect("first frame");
        let ptr = first.as_ptr() as usize;
        p.recycle(first);
        // Keep draining and recycling: the buffer set closes over the
        // pool, so the original pointer must come back around. Bounded
        // wait — decode-ahead may have allocated a few fresh frames
        // before the recycle landed.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reused = false;
        while Instant::now() < deadline && !reused {
            if let Some(f) = p.take_frame() {
                reused = f.as_ptr() as usize == ptr;
                p.recycle(f);
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(reused, "a recycled buffer never came back around");
    }

    /// Bounded wait until the decoder has buffered at least `n` frames
    /// (replaces the contention-flaky fixed sleeps; T1).
    fn wait_buffered(p: &SeekablePlayer, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if p.buffered() >= n {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Drop must wake a reader parked on the full-queue condvar — the
    /// in-process shape of the CLI player's leaked-reader bug (threads
    /// pinning frame buffers after every selection change).
    #[test]
    fn dropped_seekable_player_releases_its_reader() {
        let Some(clip) = test_clip("drop.mp4", 4) else {
            return;
        };
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&meta(&clip))).expect("spawn");
        // Reader parks once the queue fills — wait for that state
        // (bounded, not a fixed sleep; T1).
        assert!(
            wait_buffered(&p, LIVE_QUEUE_DEPTH, Duration::from_secs(15)),
            "queue never filled"
        );
        let shared = Arc::downgrade(&p.shared);
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            shared.upgrade().is_none(),
            "reader thread leaked after drop"
        );
    }

    /// Real camera/encoder output tags colorspace and range (bt709/tv);
    /// the lavfi test clips don't, which is how a buffersrc declared
    /// without color properties slipped through. The graph must accept
    /// tagged frames without renegotiation stalls on either chain.
    #[test]
    fn color_tagged_source_still_delivers_frames() {
        if !crate::have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_seekable_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("tagged.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=320x180:rate=30")
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .args(["-colorspace", "bt709", "-color_primaries", "bt709"])
                .args(["-color_trc", "bt709", "-color_range", "tv"])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate tagged test clip");
        }
        // hw chain (VT decode + scale_vt) and sw chain (no pix_fmt in
        // meta forces the software scale path) — both must flow.
        let mut sw_meta = meta(&clip);
        sw_meta.pix_fmt = None;
        for (label, m) in [("hw", meta(&clip)), ("sw", sw_meta)] {
            let p = SeekablePlayer::spawn(&clip, 320, 180, 0.4, Some(&m)).expect("spawn");
            assert!(
                take_one(&p, Duration::from_secs(3)).is_some(),
                "{label} chain: no first frame from color-tagged source"
            );
            let t0 = Instant::now();
            let mut frames = 0u32;
            while t0.elapsed() < Duration::from_secs(1) {
                if p.take_frame().is_some() {
                    frames += 1;
                }
                thread::sleep(Duration::from_millis(2));
            }
            assert!(
                (20..=45).contains(&frames),
                "{label} chain: expected ~30 paced frames from tagged source, got {frames}"
            );
            assert!(!p.failed(), "{label} chain: reader reported failure");
        }
    }

    /// The whole point of the port: `seek()` jumps the SAME stream — no
    /// respawn — and frames from the new position flow within a moment.
    /// Exact mode must land at the target even when the clip has a single
    /// keyframe (worst case: decode-forward across the whole GOP).
    #[test]
    fn seek_jumps_in_place_without_respawn() {
        let Some(clip) = test_clip("seek.mp4", 8) else {
            return;
        };
        let p = SeekablePlayer::spawn(&clip, 320, 180, 0.2, Some(&meta(&clip))).expect("spawn");
        assert!(
            take_one(&p, Duration::from_secs(3)).is_some(),
            "no first frame"
        );
        assert!(
            p.position() < 2.0,
            "started near the head, got {}",
            p.position()
        );

        p.seek(6.0, true);
        assert!(
            take_one(&p, Duration::from_secs(5)).is_some(),
            "no frame after exact seek"
        );
        let pos = p.position();
        assert!(
            (5.8..=7.0).contains(&pos),
            "exact seek should land at ~6s, position {pos}"
        );

        // And back: a backward seek on the same stream, still alive.
        p.seek(0.5, true);
        assert!(
            take_one(&p, Duration::from_secs(5)).is_some(),
            "no frame after backward seek"
        );
        let pos = p.position();
        assert!(pos < 2.0, "backward seek should rewind, position {pos}");
        assert!(!p.failed(), "reader reported failure");
    }

    /// Files with a non-zero stream start_time (edit lists — everywhere in
    /// phone/camera/QuickTime footage) once opened the live stream on a
    /// DIFFERENT keyframe than the static thumb: the CLI `-ss` thumbnail
    /// seek is content-relative (relative to start_time) while libav's
    /// `avformat_seek_file` uses absolute stream timestamps. The player now
    /// normalizes to content-relative time, so `spawn(seek)` lands on the
    /// same keyframe the thumb grabbed and `position()` reports 0-based
    /// time. Fixture: 10s, keyframes every 3s, timestamps shifted +5s.
    #[test]
    fn start_time_offset_is_content_relative() {
        if !crate::have_binary("ffmpeg") {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join("sb_media_seekable_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("offset_g90.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=10:size=320x180:rate=30")
                .args([
                    "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                    // keyframe every 3s, and shift all timestamps by +5s so
                    // the stream start_time is 5.0 (not 0).
                    "-g", "90", "-sc_threshold", "0", "-output_ts_offset", "5.0",
                ])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate offset test clip");
        }

        // Content-relative seek to 4.0s → nearest keyframe ≤ 4.0 in content
        // time is 3.0 (absolute 8.0). The old absolute-time seek landed on
        // content 0 / reported ~5.0 (the start_time) — the visible jump.
        let p = SeekablePlayer::spawn(&clip, 320, 180, 4.0, Some(&meta(&clip))).expect("spawn");
        assert!(
            take_one(&p, Duration::from_secs(3)).is_some(),
            "no first frame from offset source"
        );
        let pos = p.position();
        assert!(
            (2.5..=3.5).contains(&pos),
            "start-time offset must be content-relative: expected ~3.0, got {pos}"
        );
        assert!(!p.failed(), "reader reported failure");
    }
}

