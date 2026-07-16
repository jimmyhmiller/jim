//! Shared "project name → id" resolution against the running GUI (it
//! owns the project list). Used by `cmd_msg`, `cmd_review`, and any
//! other command that mirrors bus traffic onto a project channel.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

pub fn resolve_project_id(name: &str) -> Result<u64, String> {
    let sock = socket_path().ok_or_else(|| "$HOME not set".to_string())?;
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {} (is the app running?)", sock.display(), e))?;
    stream
        .write_all(br#"{"action":"list_projects"}"#)
        .map_err(|e| format!("write: {}", e))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .map_err(|e| format!("read: {}", e))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("bad response: {}", e))?;
    let projects = parsed
        .get("projects")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "no projects in response".to_string())?;
    for p in projects {
        let pname = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if pname.eq_ignore_ascii_case(name) {
            return p
                .get("id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "project has no id".to_string());
        }
    }
    Err(format!("no project named `{}`", name))
}
