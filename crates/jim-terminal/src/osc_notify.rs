//! Terminal notifications (OSC 9 / 777) and iTerm notification capability queries.
//! libghostty-vt has no notification callback, so watch the live PTY stream.

#[derive(Default)]
pub struct OscNotifyWatcher {
    state: State,
    buf: Vec<u8>,
}

#[derive(Default)]
enum State {
    #[default]
    Normal,
    Esc,
    Osc,
    OscEsc,
    Discard,
    DiscardEsc,
}

pub enum Event {
    Notification,
    Capabilities,
}

impl OscNotifyWatcher {
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(Event)) {
        for &b in bytes {
            match self.state {
                State::Normal => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b']' => State::Osc,
                        0x1b => State::Esc,
                        _ => State::Normal,
                    };
                }
                State::Osc => match b {
                    0x07 => self.finish(&mut emit),
                    0x1b => self.state = State::OscEsc,
                    _ if self.buf.len() < 16 * 1024 => self.buf.push(b),
                    _ => {
                        self.buf.clear();
                        self.state = State::Discard;
                    }
                },
                State::OscEsc => {
                    if b == b'\\' {
                        self.finish(&mut emit);
                    } else {
                        self.buf.clear();
                        self.state = State::Discard;
                    }
                }
                State::Discard => match b {
                    0x07 => self.state = State::Normal,
                    0x1b => self.state = State::DiscardEsc,
                    _ => {}
                },
                State::DiscardEsc => {
                    self.state = if b == b'\\' || b == 0x07 {
                        State::Normal
                    } else {
                        State::Discard
                    };
                }
            }
        }
    }

    fn finish(&mut self, emit: &mut impl FnMut(Event)) {
        if self.buf == b"1337;Capabilities" {
            emit(Event::Capabilities);
        } else if self.buf.starts_with(b"9;") && self.buf.len() > 2 {
            // OSC 9;4 is a progress indicator, not a notification.
            if !self.buf[2].is_ascii_digit() || self.buf.get(3) != Some(&b';') {
                emit(Event::Notification);
            }
        } else if let Some(payload) = self.buf.strip_prefix(b"777;notify;") {
            if payload.contains(&b';') && payload.iter().any(|&b| b != b';') {
                emit(Event::Notification);
            }
        }
        self.buf.clear();
        self.state = State::Normal;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_and_queries_survive_every_chunk_boundary() {
        let bytes = b"\x1b]1337;Capabilities\x1b\\\x1b]9;Session done\x07\x1b]777;notify;OpenCode;Permission needs input\x1b\\";
        for split in 0..=bytes.len() {
            let mut watcher = OscNotifyWatcher::default();
            let mut notifications = 0;
            let mut queries = 0;
            let mut emit = |event| match event {
                Event::Notification => notifications += 1,
                Event::Capabilities => queries += 1,
            };
            watcher.feed(&bytes[..split], &mut emit);
            watcher.feed(&bytes[split..], &mut emit);
            assert_eq!((notifications, queries), (2, 1), "split {split}");
        }
    }

    #[test]
    fn ignores_bells_titles_clipboard_progress_and_empty_notifications() {
        let mut watcher = OscNotifyWatcher::default();
        watcher.feed(b"\x07\x1b]2;title\x07\x1b]52;c;YQ==\x07\x1b]9;4;1;50\x07\x1b]9;\x07\x1b]777;notify;;\x07", |_| panic!("unexpected event"));
    }

    #[test]
    fn oversized_sequence_is_discarded_and_parser_recovers() {
        let mut watcher = OscNotifyWatcher::default();
        watcher.feed(b"\x1b]9;", |_| panic!("incomplete event"));
        watcher.feed(&vec![b'x'; 20 * 1024], |_| panic!("oversized event"));
        let mut count = 0;
        watcher.feed(b"\x07\x1b]9;done\x07", |_| count += 1);
        assert_eq!(count, 1);
    }
}
