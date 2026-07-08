use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::thread;

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
}

/// Streams paths from stdin as they arrive — newline- or NUL-delimited,
/// never waiting for EOF (PLAN.md §3 pillar 3). Returns None when stdin is
/// a TTY. Directories walk recursively when `recurse` is on and are
/// skipped otherwise; non-video files are skipped either way.
pub fn spawn_stdin_reader(recurse: bool) -> Option<Receiver<Ingested>> {
    if io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
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
    });
    Some(rx)
}

/// CLI-argument source: same semantics as stdin, but from `argv`. Takes
/// priority over stdin when both are present.
pub fn spawn_args_reader(paths: Vec<PathBuf>, recurse: bool) -> Receiver<Ingested> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for p in paths {
            if handle_path(&tx, p, recurse).is_err() {
                return;
            }
        }
    });
    rx
}

fn send_path(
    tx: &Sender<Ingested>,
    mut bytes: &[u8],
    recurse: bool,
) -> Result<(), SendError<Ingested>> {
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
fn handle_path(
    tx: &Sender<Ingested>,
    path: PathBuf,
    recurse: bool,
) -> Result<(), SendError<Ingested>> {
    let meta = std::fs::metadata(&path);
    if meta.as_ref().is_ok_and(|m| m.is_dir()) {
        if recurse {
            walk_dir(tx, &path, 0)?;
        }
        return Ok(());
    }
    if !is_video(&path) {
        return Ok(());
    }
    let cloud = is_cloud_placeholder(&path, meta.as_ref().ok());
    tx.send(Ingested {
        path,
        readable: meta.is_ok(),
        cloud,
    })
}

/// Streaming recursive walk (depth-capped; hidden entries and symlinked
/// directories are skipped — no cycles). `.name.icloud` download stubs
/// resolve back to their original video name as cloud placeholders.
fn walk_dir(tx: &Sender<Ingested>, dir: &Path, depth: usize) -> Result<(), SendError<Ingested>> {
    if depth > 24 {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = rd.flatten().collect();
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
                    })?;
                }
            }
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk_dir(tx, &e.path(), depth + 1)?;
        } else if ft.is_file() && is_video(&e.path()) {
            let p = e.path();
            let meta = std::fs::metadata(&p);
            let cloud = is_cloud_placeholder(&p, meta.as_ref().ok());
            tx.send(Ingested {
                path: p,
                readable: meta.is_ok(),
                cloud,
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
