//! Streaming OSC 52 (clipboard write) parser + system-clipboard write.
//!
//! OSC 52 is how a program running inside the terminal — Claude Code,
//! vim's `+clipboard` fallback, tmux `copy-pipe`, ssh sessions — asks the
//! *terminal* to put text on the system clipboard, since the program
//! itself may be on the far side of a pty/ssh hop and can't reach the
//! pasteboard:
//!
//!   ESC `]` `52` `;` <Pc> `;` <base64-payload> ST
//!
//! where ST is BEL (`0x07`) or ESC `\`, and `Pc` is the target selection
//! (`c` clipboard, `p`/`s` primary/select, `0`-`7` cut buffers; empty
//! means the default). macOS has one pasteboard, so we write every
//! selection to it and ignore `Pc` entirely.
//!
//! libghostty-vt parses OSC 52 but exposes no clipboard callback on
//! `Terminal`, so — exactly like [`crate::osc7`] and
//! [`crate::command_watch`] — we run our own tolerant state machine over
//! the same byte stream the worker hands to `vt_write`. Bytes may split
//! across `feed` calls.
//!
//! **Reads are refused.** A payload of `?` is a request to *send the
//! clipboard back to the program*, which lets anything that can write
//! bytes to the pty (including `cat`ing a hostile file) exfiltrate
//! whatever you last copied. We drop those without replying, which
//! programs treat as "terminal doesn't support clipboard reads".

use std::io::Write;
use std::process::{Command, Stdio};

/// Cap on the accumulated base64 payload. ~3 MiB of base64 decodes to
/// ~2.25 MiB of text — far past any real copy, and a malformed sequence
/// can't grow the buffer without bound.
const MAX_PAYLOAD: usize = 3 * 1024 * 1024;

#[derive(Debug)]
enum State {
    Normal,
    Esc,
    OscOpen,
    /// Inside a 52 payload (after `ESC ] 52 ;`). Accumulates the
    /// `<Pc>;<base64>` body until ST.
    Payload,
    /// Inside an OSC we don't care about. Eat until ST.
    Other,
    /// Saw ESC inside a 52 payload; `\` next ⇒ ST.
    PayloadEsc,
    OtherEsc,
}

pub struct Osc52Watcher {
    state: State,
    buf: Vec<u8>,
}

impl Default for Osc52Watcher {
    fn default() -> Self {
        Self {
            state: State::Normal,
            buf: Vec::with_capacity(256),
        }
    }
}

impl Osc52Watcher {
    /// Feed PTY bytes. For each completed OSC 52 *write*, `on_copy` is
    /// invoked with the decoded text. Clipboard *queries* (`?`), empty
    /// payloads (xterm's "clear the selection" — we'd rather not clobber
    /// the user's clipboard on a stray sequence) and payloads that don't
    /// decode are silently dropped.
    pub fn feed<F: FnMut(String)>(&mut self, bytes: &[u8], mut on_copy: F) {
        for &b in bytes {
            match self.state {
                State::Normal => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    if b == b']' {
                        self.buf.clear();
                        self.state = State::OscOpen;
                    } else {
                        self.state = State::Normal;
                    }
                }
                State::OscOpen => {
                    if b == b';' {
                        if self.buf == b"52" {
                            self.buf.clear();
                            self.state = State::Payload;
                        } else {
                            self.buf.clear();
                            self.state = State::Other;
                        }
                    } else if b == 0x07 {
                        self.buf.clear();
                        self.state = State::Normal;
                    } else if self.buf.len() < 8 {
                        self.buf.push(b);
                    } else {
                        self.state = State::Other;
                        self.buf.clear();
                    }
                }
                State::Payload => match b {
                    0x07 => {
                        self.emit(&mut on_copy);
                        self.state = State::Normal;
                    }
                    0x1b => {
                        self.state = State::PayloadEsc;
                    }
                    _ => {
                        if self.buf.len() < MAX_PAYLOAD {
                            self.buf.push(b);
                        } else {
                            self.buf.clear();
                            self.state = State::Other;
                        }
                    }
                },
                State::PayloadEsc => {
                    if b == b'\\' {
                        self.emit(&mut on_copy);
                        self.state = State::Normal;
                    } else if self.buf.len() + 2 <= MAX_PAYLOAD {
                        self.buf.push(0x1b);
                        self.buf.push(b);
                        self.state = State::Payload;
                    } else {
                        self.buf.clear();
                        self.state = State::Other;
                    }
                }
                State::Other => match b {
                    0x07 => self.state = State::Normal,
                    0x1b => self.state = State::OtherEsc,
                    _ => {}
                },
                State::OtherEsc => {
                    if b == b'\\' {
                        self.state = State::Normal;
                    } else {
                        self.state = State::Other;
                    }
                }
            }
        }
    }

    fn emit<F: FnMut(String)>(&mut self, on_copy: &mut F) {
        let payload = std::mem::take(&mut self.buf);
        let Ok(s) = std::str::from_utf8(&payload) else {
            return;
        };
        // Body is "<Pc>;<base64>". A missing `;` is malformed — xterm
        // would read it as an empty selection list, but every real
        // emitter sends the separator, so treat it as junk.
        let Some((_selection, data)) = s.split_once(';') else {
            return;
        };
        if data.is_empty() || data == "?" {
            // Clear request / read query — see the module docs.
            return;
        }
        let Some(bytes) = crate::base64::decode(data) else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        // Lossy: the payload is nominally UTF-8, but a program pushing
        // raw bytes shouldn't lose the whole copy over one bad sequence.
        on_copy(String::from_utf8_lossy(&bytes).into_owned());
    }
}

/// Put `text` on the macOS pasteboard. Shells out to `pbcopy` rather
/// than using `arboard` because the caller is the terminal's worker
/// thread and AppKit pasteboard access off the main thread isn't safe;
/// a child process sidesteps that. (Same trick as
/// `jim_widget::subprocess::clipboard_set`.) Returns whether it worked.
pub fn set_system_clipboard(text: &str) -> bool {
    let mut child = match Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        // stdin dropped here -> EOF -> pbcopy commits.
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &[u8]) -> Vec<String> {
        let mut w = Osc52Watcher::default();
        let mut out = Vec::new();
        w.feed(input, |t| out.push(t));
        out
    }

    #[test]
    fn bel_terminated_write() {
        // "hello" == aGVsbG8=
        assert_eq!(run(b"\x1b]52;c;aGVsbG8=\x07"), vec!["hello".to_string()]);
    }

    #[test]
    fn st_terminated_write() {
        assert_eq!(run(b"\x1b]52;c;aGVsbG8=\x1b\\"), vec!["hello".to_string()]);
    }

    #[test]
    fn empty_selection_field() {
        // Pc omitted — common from tmux/vim.
        assert_eq!(run(b"\x1b]52;;aGVsbG8=\x07"), vec!["hello".to_string()]);
    }

    #[test]
    fn multi_selection_field() {
        assert_eq!(run(b"\x1b]52;c0;aGVsbG8=\x07"), vec!["hello".to_string()]);
    }

    #[test]
    fn query_is_refused() {
        assert!(run(b"\x1b]52;c;?\x07").is_empty());
    }

    #[test]
    fn empty_payload_does_not_clobber() {
        assert!(run(b"\x1b]52;c;\x07").is_empty());
    }

    #[test]
    fn invalid_base64_dropped() {
        assert!(run(b"\x1b]52;c;!!!not-b64!!!\x07").is_empty());
    }

    #[test]
    fn ignores_other_oscs() {
        let s = b"\x1b]7;file:///x\x07\x1b]133;D;0\x07\x1b]52;c;aGVsbG8=\x07";
        assert_eq!(run(s), vec!["hello".to_string()]);
    }

    #[test]
    fn split_across_feeds() {
        let mut w = Osc52Watcher::default();
        let mut out = Vec::new();
        w.feed(b"\x1b]52;c;aGVs", |t| out.push(t));
        w.feed(b"bG8=\x07rest", |t| out.push(t));
        assert_eq!(out, vec!["hello".to_string()]);
    }

    #[test]
    fn multiline_payload() {
        // "a\nb" == YQpi
        assert_eq!(run(b"\x1b]52;c;YQpi\x07"), vec!["a\nb".to_string()]);
    }

    #[test]
    fn oversized_payload_resyncs() {
        let mut s = b"\x1b]52;c;".to_vec();
        s.extend(std::iter::repeat_n(b'A', MAX_PAYLOAD + 16));
        s.extend_from_slice(b"\x07");
        // Dropped, and the watcher is back in sync for the next one.
        s.extend_from_slice(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(run(&s), vec!["hello".to_string()]);
    }
}
