//! Spike benchmark for switchblade DESIGN.md §15 "Low-latency seek", port step 1.
//!
//! Question under test: with a RESIDENT demuxer + VideoToolbox decoder session
//! (in-process libav instead of an ffmpeg CLI respawn), what does an arbitrary
//! seek cost on 4K sources?
//!
//! Measures, per clip, hw then sw:
//!   - session init (open + find stream + decoder + VT hwdevice) — paid ONCE
//!   - cold first frame ≈ what every CLI respawn pays today, minus process
//!     exec + reprobe
//!   - 10 random seeks: avformat_seek_file(BACKWARD) + flush + decode forward
//!     until frame pts >= target; the landed frame's hw→sw download is timed
//!     inline (upper bound — the real pipeline scales on-GPU first)
//!
//! Written against rsmpeg's raw ffi (mirrors ffmpeg's doc/examples/hw_decode.c)
//! — doubles as proof the binding crate builds/links against brew ffmpeg 8.x.

use rsmpeg::ffi;
use std::ffi::CString;
use std::ptr;
use std::time::Instant;

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

struct Session {
    fmt: *mut ffi::AVFormatContext,
    dec: *mut ffi::AVCodecContext,
    stream_index: i32,
    time_base: ffi::AVRational,
    duration_s: f64,
    hw: bool,
}

unsafe fn open_session(path: &str, use_hw: bool) -> Result<Session, String> {
    unsafe {
        let cpath = CString::new(path).unwrap();
        let mut fmt: *mut ffi::AVFormatContext = ptr::null_mut();
        if ffi::avformat_open_input(&mut fmt, cpath.as_ptr(), ptr::null(), ptr::null_mut()) < 0 {
            return Err("open_input failed".into());
        }
        if ffi::avformat_find_stream_info(fmt, ptr::null_mut()) < 0 {
            return Err("find_stream_info failed".into());
        }
        let mut decoder: *const ffi::AVCodec = ptr::null();
        let stream_index =
            ffi::av_find_best_stream(fmt, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, &mut decoder, 0);
        if stream_index < 0 {
            return Err("no video stream".into());
        }
        let stream = *(*fmt).streams.add(stream_index as usize);
        let dec = ffi::avcodec_alloc_context3(decoder);
        if ffi::avcodec_parameters_to_context(dec, (*stream).codecpar) < 0 {
            return Err("params_to_context failed".into());
        }
        let mut hw_ok = false;
        if use_hw {
            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            if ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                ptr::null(),
                ptr::null_mut(),
                0,
            ) == 0
            {
                (*dec).hw_device_ctx = ffi::av_buffer_ref(hw_device);
                (*dec).get_format = Some(get_hw_format);
                hw_ok = true;
            }
        }
        if ffi::avcodec_open2(dec, decoder, ptr::null_mut()) < 0 {
            return Err("avcodec_open2 failed".into());
        }
        let duration_s = (*fmt).duration as f64 / ffi::AV_TIME_BASE as f64;
        Ok(Session {
            fmt,
            dec,
            stream_index,
            time_base: (*stream).time_base,
            duration_s,
            hw: hw_ok,
        })
    }
}

/// Decode forward until a frame with pts >= target lands. Returns
/// (frames_decoded, landed_pts_seconds, frame_was_hw, download_ms).
unsafe fn decode_until(s: &Session, target_s: f64) -> Result<(u32, f64, bool, f64), String> {
    unsafe {
        let mut pkt = ffi::av_packet_alloc();
        let mut frame = ffi::av_frame_alloc();
        let tb = s.time_base.num as f64 / s.time_base.den as f64;
        let mut decoded = 0u32;
        let mut result: Result<(u32, f64, bool, f64), String> = Err("EOF before target".into());
        'outer: while ffi::av_read_frame(s.fmt, pkt) >= 0 {
            if (*pkt).stream_index != s.stream_index {
                ffi::av_packet_unref(pkt);
                continue;
            }
            if ffi::avcodec_send_packet(s.dec, pkt) < 0 {
                ffi::av_packet_unref(pkt);
                result = Err("send_packet failed".into());
                break;
            }
            ffi::av_packet_unref(pkt);
            loop {
                let r = ffi::avcodec_receive_frame(s.dec, frame);
                if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    result = Err("receive_frame failed".into());
                    break 'outer;
                }
                decoded += 1;
                let pts = (*frame).best_effort_timestamp;
                let pts_s = pts as f64 * tb;
                if pts != ffi::AV_NOPTS_VALUE && pts_s >= target_s {
                    let is_hw = (*frame).format == ffi::AV_PIX_FMT_VIDEOTOOLBOX;
                    let mut dl_ms = 0.0;
                    if is_hw {
                        // Download the landed frame — full-res upper bound.
                        let mut sw = ffi::av_frame_alloc();
                        let t = Instant::now();
                        let tr = ffi::av_hwframe_transfer_data(sw, frame, 0);
                        dl_ms = t.elapsed().as_secs_f64() * 1e3;
                        if tr < 0 {
                            dl_ms = -1.0;
                        }
                        ffi::av_frame_free(&mut sw);
                    }
                    ffi::av_frame_unref(frame);
                    result = Ok((decoded, pts_s, is_hw, dl_ms));
                    break 'outer;
                }
                ffi::av_frame_unref(frame);
            }
        }
        ffi::av_packet_free(&mut pkt);
        ffi::av_frame_free(&mut frame);
        result
    }
}

unsafe fn seek_to(s: &Session, target_s: f64) -> Result<(), String> {
    unsafe {
        let ts = (target_s / (s.time_base.num as f64 / s.time_base.den as f64)) as i64;
        if ffi::avformat_seek_file(
            s.fmt,
            s.stream_index,
            i64::MIN,
            ts,
            ts,
            ffi::AVSEEK_FLAG_BACKWARD as i32,
        ) < 0
        {
            return Err("seek failed".into());
        }
        ffi::avcodec_flush_buffers(s.dec);
        Ok(())
    }
}

fn bench(path: &str, use_hw: bool, n_seeks: u32) {
    unsafe {
        println!(
            "\n=== {path}\n    mode: {}",
            if use_hw { "videotoolbox" } else { "software" }
        );
        let t0 = Instant::now();
        let s = match open_session(path, use_hw) {
            Ok(s) => s,
            Err(e) => {
                println!("    ERROR: {e}");
                return;
            }
        };
        let init = t0.elapsed();
        let t1 = Instant::now();
        match decode_until(&s, 0.0) {
            Ok((_, _, hw, _)) => println!(
                "    session init {:6.1}ms   cold first frame +{:.1}ms   (hw session: {})",
                init.as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3,
                if hw && s.hw { "yes" } else { "NO" },
            ),
            Err(e) => {
                println!("    first-frame ERROR: {e}");
                return;
            }
        }
        // Deterministic pseudo-random positions across the duration.
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut total = 0.0f64;
        let mut worst = 0.0f64;
        let mut ok = 0u32;
        for i in 0..n_seeks {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let frac = 0.02 + 0.96 * ((state >> 11) as f64 / (1u64 << 53) as f64);
            let target = frac * s.duration_s;
            // Keyframe-mode first: seek + decode ONE frame (what a scrub
            // shows while the thumb is moving; mpv --hr-seek=no).
            let tk = Instant::now();
            let kf_ms = match seek_to(&s, target).and_then(|_| decode_until(&s, 0.0)) {
                Ok(_) => tk.elapsed().as_secs_f64() * 1e3,
                Err(_) => -1.0,
            };
            let t = Instant::now();
            if let Err(e) = seek_to(&s, target) {
                println!("    seek {i}: {e}");
                continue;
            }
            match decode_until(&s, target) {
                Ok((frames, landed, hw, dl_ms)) => {
                    let ms = t.elapsed().as_secs_f64() * 1e3;
                    total += ms;
                    worst = worst.max(ms);
                    ok += 1;
                    println!(
                        "  seek → {target:7.2}s   kf {kf_ms:5.1}ms   exact {ms:6.1}ms   ({frames:3} frames from keyframe, landed {landed:7.2}s, hw:{}, dl {dl_ms:.1}ms)",
                        if hw { "y" } else { "n" }
                    );
                }
                Err(e) => println!("    seek {i} decode: {e}"),
            }
        }
        if ok > 0 {
            println!("    avg {:6.1}ms   worst {:6.1}ms", total / ok as f64, worst);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: seek-bench <clip> [clip...]");
        std::process::exit(2);
    }
    for path in &args {
        bench(path, true, 10);
        bench(path, false, 10);
    }
}
