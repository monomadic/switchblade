use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// A path received from stdin, pre-checked off the main thread.
pub struct Ingested {
    pub path: PathBuf,
    pub readable: bool,
}

/// Streams paths from stdin as they arrive — newline- or NUL-delimited,
/// never waiting for EOF (PLAN.md §3 pillar 3). Returns None when stdin is
/// a TTY (demo mode).
pub fn spawn_stdin_reader() -> Option<Receiver<Ingested>> {
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
                            if send_path(&tx, &pending[start..i]).is_err() {
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
        let _ = send_path(&tx, &remainder);
    });
    Some(rx)
}

fn send_path(
    tx: &mpsc::Sender<Ingested>,
    mut bytes: &[u8],
) -> Result<(), mpsc::SendError<Ingested>> {
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

    // stat here, off the main thread, so the UI never blocks on slow disks.
    let readable = std::fs::metadata(&path).is_ok();
    tx.send(Ingested { path, readable })
}
