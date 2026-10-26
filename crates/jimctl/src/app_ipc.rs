//! One-shot request to the running app's IPC socket, for commands that
//! build requests as JSON (no per-module wire enum) and may want the
//! reply body back.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

/// Send `req` and, when `reply` is set, read and parse the JSON response.
pub fn request(req: &Value, reply: bool) -> Result<Option<Value>, String> {
    let sock = socket_path().ok_or("$HOME not set")?;
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {e} (is Jim running?)", sock.display()))?;
    let bytes = serde_json::to_vec(req).map_err(|e| format!("serialize: {e}"))?;
    stream
        .write_all(&bytes)
        .map_err(|e| format!("write: {e}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    if !reply {
        return Ok(None);
    }
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|e| format!("bad response {body:?}: {e}"))
}
