//! `jimctl web` — open a web pane in the running Jim.
//!
//! Exists so the in-app agent (and any script) can open pages: it shells out,
//! and this is a stable command rather than a hand-rolled JSON blob over the
//! socket.
//!
//! Usage:
//!     jimctl web <url> [--project NAME]
//!
//! The URL is normalised the same way the command palette does it, so
//! `example.com`, `localhost:3000` and `file:///tmp/x.html` all work.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

/// Same rules as the palette's `as_url`: explicit scheme wins, `localhost`
/// gets http, anything host-shaped gets https.
pub fn normalize(raw: &str) -> Option<String> {
    let q = raw.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    if q.starts_with("http://") || q.starts_with("https://") || q.starts_with("file://") {
        return Some(q.to_string());
    }
    let host = q.split('/').next().unwrap_or(q);
    if host == "localhost" || host.starts_with("localhost:") {
        return Some(format!("http://{q}"));
    }
    let (before, after) = host.split_once('.')?;
    let tld_like = after.len() >= 2 && after.chars().all(|c| c.is_ascii_alphabetic() || c == '.');
    if before.is_empty() || !tld_like {
        return None;
    }
    Some(format!("https://{q}"))
}

pub fn run() -> ExitCode {
    let mut args = std::env::args().skip(2);
    let mut url: Option<String> = None;
    let mut project: Option<String> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--project" | "-p" => project = args.next(),
            "-h" | "--help" => {
                usage();
                return ExitCode::SUCCESS;
            }
            other => url = Some(other.to_string()),
        }
    }

    let Some(raw) = url else {
        usage();
        return ExitCode::from(2);
    };
    let Some(url) = normalize(&raw) else {
        eprintln!("jimctl web: {raw:?} doesn't look like a URL");
        return ExitCode::from(2);
    };

    open_url(&url, project.as_deref())
}

/// Spawn a web pane for an already-normalised URL. Shared with `jimctl open`,
/// which routes URLs here instead of trying to canonicalise them as paths.
pub fn open_url(url: &str, project: Option<&str>) -> ExitCode {
    let mut payload = String::from(r#"{"action":"spawn_pane","kind":"webview""#);
    if let Some(p) = project {
        payload.push_str(&format!(r#","project":{}"#, json_str(p)));
    }
    payload.push_str(&format!(r#","config":{{"url":{}}}}}"#, json_str(url)));

    let Some(sock) = socket_path() else {
        eprintln!("jimctl web: no $HOME");
        return ExitCode::FAILURE;
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jimctl web: is Jim running? ({sock:?}: {e})");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = writeln!(stream, "{payload}") {
        eprintln!("jimctl web: {e}");
        return ExitCode::FAILURE;
    }
    println!("opening {url}");
    ExitCode::SUCCESS
}

/// Minimal JSON string escaping — jimctl deliberately has no serde_json.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn usage() {
    eprintln!("usage: jimctl web <url> [--project NAME]");
    eprintln!();
    eprintln!("Opens a web pane in the running Jim.");
    eprintln!("  jimctl web example.com");
    eprintln!("  jimctl web localhost:3000 --project Recursion");
    eprintln!("  jimctl web file:///tmp/report.html");
}
