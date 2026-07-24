use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, SendError, SyncSender};
use std::thread;
use std::time::{Instant, SystemTime};

/// Ingest channels are bounded (P0.3): a fast directory walk during GPU
/// init used to queue every entry in memory at once. A full channel now
/// parks the *reader thread* — never the UI, which drains with a
/// per-frame budget and catches up frame by frame.
const CHANNEL_CAP: usize = 1024;

/// Extensions we treat as video. Anything else piped/passed in is
/// silently skipped — unsupported files never become tiles.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "webm", "mkv", "avi", "wmv", "flv", "f4v", "mpg", "mpeg", "m2v",
    "ts", "m2ts", "mts", "3gp", "3g2", "ogv", "vob", "mxf", "y4m", "gif",
];

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|v| e.eq_ignore_ascii_case(v)))
}

/// A path received from stdin, pre-checked off the main thread.
pub struct Ingested {
    pub path: PathBuf,
    pub readable: bool,
    /// iCloud placeholder: the file exists in the tree but its data is in
    /// the cloud. Reading it would trigger a download, so we never do.
    pub cloud: bool,
    /// Creation time (birthtime where the OS has one, mtime otherwise),
    /// read from the stat the gatekeeper already performs — carried so
    /// sorted ingest (`--sort newest`) never stats on the render thread.
    pub created: Option<SystemTime>,
    /// Cache-key inputs (size, mtime secs) from that same stat. Carried
    /// for the same reason `created` is, and for a sharper one: without
    /// them a cached-meta lookup has to stat the clip's own volume, which
    /// on SMB is a ~400 ms round-trip (perf review 05 §3). With them the
    /// lookup only reads the local cache entry. `None` for cloud
    /// placeholders and paths that never resolved.
    pub fp: Option<(u64, u64)>,
}

/// The gatekeeper's cheap content check: a valid video extension is not
/// proof of a video. Zero-byte files and files whose first bytes don't
/// match their extension's container family (an HTML error page saved as
/// .mp4, a text file renamed) are rejected before they ever claim a chip.
/// One 16-byte read on the ingest thread — never a probe, never a decode;
/// truncated-but-well-headed files still pass and are caught later when
/// their thumbnail generation fails (the app drops them from the grid).
/// Container families we can't fingerprint confidently fail open.
fn plausible_video(path: &Path, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return true;
    };
    let mut head = [0u8; 16];
    let n = match std::fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(n) => n,
        Err(_) => return true, // unreadable here ≠ invalid; later stages decide
    };
    let head = &head[..n];
    let has = |sig: &[u8], at: usize| head.len() >= at + sig.len() && &head[at..at + sig.len()] == sig;
    match ext.to_ascii_lowercase().as_str() {
        // ISO BMFF / QuickTime: box size then a known top-level box type.
        "mp4" | "m4v" | "mov" | "qt" | "3gp" | "3g2" | "f4v" => [
            &b"ftyp"[..], b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot", b"uuid",
        ]
        .iter()
        .any(|s| has(s, 4)),
        "mkv" | "webm" => has(&[0x1A, 0x45, 0xDF, 0xA3], 0), // EBML
        "avi" => has(b"RIFF", 0) && has(b"AVI ", 8),
        "wmv" => has(&[0x30, 0x26, 0xB2, 0x75], 0), // ASF GUID head
        "flv" => has(b"FLV", 0),
        "mpg" | "mpeg" | "m2v" | "vob" => has(&[0x00, 0x00, 0x01], 0),
        "ts" | "m2ts" | "mts" => has(&[0x47], 0) || has(&[0x47], 4), // sync byte (BDAV at +4)
        "ogv" => has(b"OggS", 0),
        "gif" => has(b"GIF8", 0),
        "y4m" => has(b"YUV4MPEG2", 0),
        "mxf" => has(&[0x06, 0x0E, 0x2B, 0x34], 0), // partition pack key
        _ => true,
    }
}

/// Fired after each delivered item (and once when a producer finishes),
/// so the render loop wakes for streamed paths instead of polling for
/// them (docs/perf-reviews/02-efficiency-review.md P0.2). Wakes coalesce window-side, so
/// per-item calls are cheap even on a fast directory walk.
pub type Notify = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Channel to the app plus the render-loop nudge, so no send site can
/// forget the wake.
struct Tx {
    tx: SyncSender<Ingested>,
    notify: Notify,
    /// The gatekeeper's header check (`gatekeeper` config); off = the
    /// pre-gatekeeper extension-and-exists test only.
    check: bool,
    /// Instrumentation: how many paths this thread saw / admitted /
    /// rejected, and how long its `stat` + `read_dir` + header reads
    /// took. On a slow disk that I/O time is the whole cost of getting a
    /// library on screen, and no latency metric elsewhere exposes it.
    probe: Arc<sb_media::Probe>,
}

impl Tx {
    fn send(&self, item: Ingested) -> Result<(), SendError<Ingested>> {
        self.probe.counters.ingest_admitted.fetch_add(1, Relaxed);
        self.tx.send(item)?;
        (self.notify)();
        Ok(())
    }

    /// Time a filesystem call into `ingest_io_us`.
    fn io<T>(&self, f: impl FnOnce() -> T) -> T {
        let t0 = Instant::now();
        let out = f();
        self.probe
            .counters
            .ingest_io_us
            .fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
        out
    }
}

/// Streams paths from stdin as they arrive — newline- or NUL-delimited,
/// never waiting for EOF (DESIGN.md §3 pillar 3). Returns None when stdin is
/// a TTY. Directories walk recursively when `recurse` is on and are
/// skipped otherwise; non-video files are skipped either way.
pub fn spawn_stdin_reader(
    recurse: bool,
    check: bool,
    notify: Notify,
    probe: Arc<sb_media::Probe>,
) -> Option<Receiver<Ingested>> {
    if io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
    let tx = Tx {
        tx,
        notify,
        check,
        probe,
    };
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match stdin.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&chunk[..n]);
                    let mut start = 0;
                    for i in 0..pending.len() {
                        let b = pending[i];
                        if b == b'\n' || b == b'\0' {
                            if send_path(&tx, &pending[start..i], recurse).is_err() {
                                return;
                            }
                            start = i + 1;
                        }
                    }
                    pending.drain(..start);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Flush an unterminated final path at EOF.
        let remainder = std::mem::take(&mut pending);
        let _ = send_path(&tx, &remainder, recurse);
        finish(tx);
    });
    Some(rx)
}

/// CLI-argument source: same semantics as stdin, but from `argv`. Takes
/// priority over stdin when both are present.
pub fn spawn_args_reader(
    paths: Vec<PathBuf>,
    recurse: bool,
    check: bool,
    notify: Notify,
    probe: Arc<sb_media::Probe>,
) -> Receiver<Ingested> {
    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
    let tx = Tx {
        tx,
        notify,
        check,
        probe,
    };
    thread::spawn(move || {
        for p in paths {
            if handle_path(&tx, p, recurse).is_err() {
                return;
            }
        }
        finish(tx);
    });
    rx
}

/// One directory's own video files, no recursion — the "browse parent
/// dir" (siblings) view. Streams like every other source; iCloud stubs
/// in the directory still resolve to placeholders.
pub fn spawn_dir_reader(
    dir: PathBuf,
    check: bool,
    notify: Notify,
    probe: Arc<sb_media::Probe>,
) -> Receiver<Ingested> {
    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAP);
    let tx = Tx {
        tx,
        notify,
        check,
        probe,
    };
    thread::spawn(move || {
        let _ = walk_dir(&tx, &dir, 0, 0);
        finish(tx);
    });
    rx
}

/// Producer done: drop the sender first, then nudge the loop once so the
/// app sees `Disconnected` promptly (closes the ingest state, clears
/// `pending_reselect`) instead of on the next idle tick.
fn finish(tx: Tx) {
    let notify = tx.notify.clone();
    drop(tx);
    notify();
}

fn send_path(tx: &Tx, mut bytes: &[u8], recurse: bool) -> Result<(), SendError<Ingested>> {
    if bytes.last() == Some(&b'\r') {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.is_empty() {
        return Ok(());
    }
    // Unix paths are bytes; don't force UTF-8.
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8_lossy(bytes).into_owned());
    handle_path(tx, path, recurse)
}

/// Route one input path: videos ingest, directories walk (when recurse
/// is on) or vanish, everything else vanishes. All stat calls happen
/// here, off the main thread, so the UI never blocks on slow disks.
fn handle_path(tx: &Tx, path: PathBuf, recurse: bool) -> Result<(), SendError<Ingested>> {
    let meta = tx.io(|| std::fs::metadata(&path));
    if meta.as_ref().is_ok_and(|m| m.is_dir()) {
        if recurse {
            walk_dir(tx, &path, 0, 24)?;
        }
        return Ok(());
    }
    if !is_video(&path) {
        return Ok(());
    }
    // Counted from here: a non-video path was never a candidate, so
    // folding it into `ingest_seen` would dilute the admit/reject ratio.
    tx.probe.counters.ingest_seen.fetch_add(1, Relaxed);
    let reject = |tx: &Tx| {
        tx.probe.counters.ingest_rejected.fetch_add(1, Relaxed);
    };
    let cloud = is_cloud_placeholder(&path, meta.as_ref().ok());
    // A valid extension is not enough: a path that doesn't exist (never
    // existed, or moved away) would otherwise claim a chip and pose as a
    // playable clip. Skip it unless it's a cloud placeholder (whose real
    // file is legitimately absent until downloaded).
    if meta.is_err() && !cloud {
        reject(tx);
        return Ok(());
    }
    // Gatekeeper content check — never on cloud placeholders (reading one
    // triggers a download).
    if tx.check
        && let Ok(m) = &meta
        && !cloud
        && !tx.io(|| plausible_video(&path, m.len()))
    {
        log::info!("gatekeeper: rejecting non-video content {}", path.display());
        reject(tx);
        return Ok(());
    }
    tx.send(Ingested {
        path,
        readable: meta.is_ok(),
        cloud,
        created: meta.as_ref().ok().and_then(created_of),
        fp: meta.as_ref().ok().map(sb_media::fingerprint_key),
    })
}

/// Creation time for sorted ingest: birthtime where the filesystem keeps
/// one (APFS does), mtime otherwise.
fn created_of(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.created().or_else(|_| meta.modified()).ok()
}

/// Streaming recursive walk (depth-capped; hidden entries and symlinked
/// directories are skipped — no cycles). `.name.icloud` download stubs
/// resolve back to their original video name as cloud placeholders.
/// `max_depth` 0 lists only the directory's own files (siblings view).
fn walk_dir(
    tx: &Tx,
    dir: &Path,
    depth: usize,
    max_depth: usize,
) -> Result<(), SendError<Ingested>> {
    if depth > max_depth {
        return Ok(());
    }
    let Ok(rd) = tx.io(|| std::fs::read_dir(dir)) else {
        return Ok(());
    };
    let mut entries: Vec<_> = tx.io(|| rd.flatten().collect());
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.first() == Some(&b'.') {
            // Hidden — but an iCloud stub stands in for a real clip.
            if let Some(orig) = bytes
                .strip_prefix(b".")
                .and_then(|b| b.strip_suffix(b".icloud"))
            {
                #[cfg(unix)]
                let orig = {
                    use std::os::unix::ffi::OsStrExt;
                    dir.join(std::ffi::OsStr::from_bytes(orig))
                };
                #[cfg(not(unix))]
                let orig = dir.join(String::from_utf8_lossy(orig).into_owned());
                if is_video(&orig) {
                    tx.send(Ingested {
                        path: orig,
                        readable: false,
                        cloud: true,
                        created: None,
                        fp: None,
                    })?;
                }
            }
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk_dir(tx, &e.path(), depth + 1, max_depth)?;
        } else if ft.is_file() && is_video(&e.path()) {
            let p = e.path();
            // The recursive walk is where a real library actually enters
            // (a directory argument, not per-file paths), so it carries
            // the same instrumentation as `handle_path` — counting only
            // that one reported an empty ingest for every dir-based run.
            tx.probe.counters.ingest_seen.fetch_add(1, Relaxed);
            let meta = tx.io(|| std::fs::metadata(&p));
            let cloud = is_cloud_placeholder(&p, meta.as_ref().ok());
            if tx.check
                && let Ok(m) = &meta
                && !cloud
                && !tx.io(|| plausible_video(&p, m.len()))
            {
                log::info!("gatekeeper: rejecting non-video content {}", p.display());
                tx.probe.counters.ingest_rejected.fetch_add(1, Relaxed);
                continue;
            }
            tx.send(Ingested {
                path: p,
                readable: meta.is_ok(),
                cloud,
                created: meta.as_ref().ok().and_then(created_of),
                fp: meta.as_ref().ok().map(sb_media::fingerprint_key),
            })?;
        }
    }
    Ok(())
}

/// Detect iCloud placeholders: APFS dataless files (evicted by
/// fileproviderd, `SF_DATALESS` in st_flags) and legacy `.name.icloud`
/// stub siblings for paths that don't resolve.
fn is_cloud_placeholder(path: &Path, meta: Option<&std::fs::Metadata>) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const SF_DATALESS: u32 = 0x4000_0000;
        if let Some(m) = meta {
            return m.st_flags() & SF_DATALESS != 0;
        }
        // File missing entirely: look for the download stub.
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            let mut stub = std::ffi::OsString::from(".");
            stub.push(name);
            stub.push(".icloud");
            return dir.join(stub).exists();
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (path, meta);
    false
}

/// Test fixture: the smallest byte string the gatekeeper accepts as an
/// ISO-BMFF (mp4/mov) file — a plausible `ftyp` box head. Shared with
/// sb-app's tests, which create fake clips the ingest readers must pass.
#[cfg(test)]
pub(crate) fn fake_mp4_bytes() -> &'static [u8] {
    b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The siblings view must list the directory's own videos only — no
    /// recursion into subdirectories, no non-video files.
    #[test]
    fn dir_reader_lists_only_siblings() {
        let dir = std::env::temp_dir().join("sb_ingest_siblings_test");
        let sub = dir.join("sub");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&sub).unwrap();
        for f in ["a.mp4", "b.mov", "note.txt"] {
            std::fs::write(dir.join(f), fake_mp4_bytes()).unwrap();
        }
        std::fs::write(sub.join("c.mp4"), fake_mp4_bytes()).unwrap();

        let rx = spawn_dir_reader(
            dir.clone(),
            true,
            std::sync::Arc::new(|| {}),
            sb_media::Probe::new(),
        );
        let mut names: Vec<String> = rx
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["a.mp4", "b.mov"]);
    }

    /// A stdin path with a valid video extension but no file behind it
    /// (never existed, or moved away) must NOT become a clip — a valid
    /// extension alone can't claim a chip. An existing video still passes.
    #[test]
    fn missing_paths_never_reach_the_grid() {
        let dir = std::env::temp_dir().join("sb_ingest_missing_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.mp4");
        std::fs::write(&real, fake_mp4_bytes()).unwrap();
        let ghost = dir.join("ghost.mp4"); // valid extension, no file

        let (raw, rx) = mpsc::sync_channel(8);
        let tx = Tx {
            tx: raw,
            notify: std::sync::Arc::new(|| {}),
            check: true,
            probe: sb_media::Probe::new(),
        };
        send_path(&tx, real.to_string_lossy().as_bytes(), false).unwrap();
        send_path(&tx, ghost.to_string_lossy().as_bytes(), false).unwrap();
        drop(tx);

        let paths: Vec<PathBuf> = rx.iter().map(|i| i.path).collect();
        assert_eq!(paths, [real]);
    }

    /// The gatekeeper's content check: a valid extension over non-video
    /// bytes (a saved error page, a renamed text file, a zero-byte stub)
    /// never claims a chip; a plausibly-headed file passes and carries
    /// its creation time for sorted ingest.
    #[test]
    fn garbage_behind_a_video_extension_never_reaches_the_grid() {
        let dir = std::env::temp_dir().join("sb_ingest_gatekeeper_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.mp4");
        std::fs::write(&good, fake_mp4_bytes()).unwrap();
        std::fs::write(dir.join("html.mp4"), b"<!DOCTYPE html><html>").unwrap();
        std::fs::write(dir.join("empty.mp4"), b"").unwrap();
        std::fs::write(dir.join("text.mkv"), b"just some notes\n").unwrap();

        let (raw, rx) = mpsc::sync_channel(8);
        let tx = Tx {
            tx: raw,
            notify: std::sync::Arc::new(|| {}),
            check: true,
            probe: sb_media::Probe::new(),
        };
        for f in ["good.mp4", "html.mp4", "empty.mp4", "text.mkv"] {
            send_path(&tx, dir.join(f).to_string_lossy().as_bytes(), false).unwrap();
        }
        drop(tx);

        let got: Vec<Ingested> = rx.iter().collect();
        assert_eq!(
            got.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            [good],
            "only the plausibly-headed file passes the gatekeeper"
        );
        assert!(got[0].created.is_some(), "the stat's creation time rides along");
    }
}
