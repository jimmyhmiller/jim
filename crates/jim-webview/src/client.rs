//! Talks to `jim-webview-host`, the process that owns CEF.
//!
//! One host process per pane. jim listens on a unix socket, the host connects
//! back, and from then on:
//!
//!   host -> jim   {"frame":{"id":123,"w":1768,"h":1164}}
//!   jim  -> host  {"resize":…} {"mouse":…} {"wheel":…} {"url":…}
//!
//! `id` is an IOSurface id: the pixels stay in GPU-shareable memory and never
//! cross the socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::mpsc::{channel, Receiver, Sender};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Frame {
    pub id: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Deserialize)]
struct FrameMsg {
    frame: Frame,
}

/// The host tells us the real URL after every navigation, so the URL bar
/// reflects link clicks and redirects rather than only what we asked for.
#[derive(Deserialize)]
struct UrlMsg {
    url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmd {
    Resize { w: f32, h: f32 },
    Mouse { x: f32, y: f32, kind: &'static str },
    Wheel { x: f32, y: f32, dx: f32, dy: f32 },
    Key {
        kind: &'static str,
        code: i32,
        text: Option<String>,
        modifiers: u32,
    },
    Back,
    Forward,
    Reload,
    Focus(bool),
    Url(String),
}

pub struct HostClient {
    child: Child,
    pub frames: Receiver<Frame>,
    pub urls: Receiver<String>,
    commands: Sender<Cmd>,
    socket_path: PathBuf,
}

impl HostClient {
    pub fn spawn(url: &str, width: f32, height: f32, scale: f32) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let host_bin = exe
            .parent()
            .ok_or("no exe dir")?
            .join("jim-webview-host");
        if !host_bin.exists() {
            return Err(format!(
                "{} is missing — make-bundle should copy it next to jim",
                host_bin.display()
            ));
        }

        let socket_path = std::env::temp_dir().join(format!(
            "jim-webview-{}-{}.sock",
            std::process::id(),
            // Distinct per pane without needing a counter resource.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener =
            UnixListener::bind(&socket_path).map_err(|e| format!("bind {socket_path:?}: {e}"))?;

        let child = Command::new(&host_bin)
            .arg(&socket_path)
            .arg(url)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(scale.to_string())
            .spawn()
            .map_err(|e| format!("spawn {host_bin:?}: {e}"))?;

        let (frame_tx, frames) = channel::<Frame>();
        let (url_tx, urls) = channel::<String>();
        let (commands, cmd_rx) = channel::<Cmd>();

        // Accept off-thread: the host takes a moment to boot Chromium and jim
        // must not block a frame on it.
        std::thread::spawn(move || {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    log::error!("[webview] host never connected: {e}");
                    return;
                }
            };
            let writer = match stream.try_clone() {
                Ok(w) => w,
                Err(e) => {
                    log::error!("[webview] could not clone host socket: {e}");
                    return;
                }
            };
            std::thread::spawn(move || write_loop(writer, cmd_rx));
            read_loop(stream, frame_tx, url_tx);
        });

        Ok(Self {
            child,
            frames,
            urls,
            commands,
            socket_path,
        })
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.commands.send(cmd);
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn read_loop(stream: UnixStream, frames: Sender<Frame>, urls: Sender<String>) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<FrameMsg>(&line) {
            if frames.send(msg.frame).is_err() {
                break;
            }
            // The frame is useless until jim draws; jim's loop is reactive.
            jim_widget::request_main_loop_wakeup();
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<UrlMsg>(&line) {
            if urls.send(msg.url).is_err() {
                break;
            }
            jim_widget::request_main_loop_wakeup();
            continue;
        }
        log::warn!("[webview] unparseable host message {line:?}");
    }
}

fn write_loop(mut stream: UnixStream, commands: Receiver<Cmd>) {
    while let Ok(cmd) = commands.recv() {
        let Ok(json) = serde_json::to_string(&cmd) else {
            continue;
        };
        if writeln!(stream, "{json}").is_err() {
            break;
        }
        let _ = stream.flush();
    }
}
