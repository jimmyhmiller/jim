//! `jim-webview-host` — runs CEF for one webview pane, out of process.
//!
//! Protocol (newline-delimited JSON over a unix socket, matching jim's other
//! subprocess widgets):
//!
//!   host -> jim   {"frame":{"id":123,"w":1768,"h":1164}}
//!   jim  -> host  {"resize":{"w":1768,"h":1164}}
//!                 {"mouse":{"x":10,"y":20,"kind":"move"|"down"|"up"}}
//!                 {"wheel":{"x":10,"y":20,"dx":0,"dy":-120}}
//!                 {"url":"https://…"}
//!
//! `id` is an IOSurface id. Pixels never cross the socket.

mod surface;

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};

use cef::args::Args;
use cef::rc::Rc as _;
use cef::*;
use serde::{Deserialize, Serialize};

use surface::SharedSurface;

#[derive(Serialize)]
struct FrameMsg {
    frame: Frame,
}

#[derive(Serialize)]
struct UrlMsg {
    url: String,
}

#[derive(Serialize)]
struct Frame {
    id: u32,
    w: u32,
    h: u32,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Command {
    Resize { w: f32, h: f32 },
    Mouse { x: f32, y: f32, kind: String },
    Wheel { x: f32, y: f32, dx: f32, dy: f32 },
    /// `kind` is "down" | "up" | "char". `text` carries the character to
    /// insert for "char"; `code` is a Windows virtual-key code.
    Key { kind: String, code: i32, text: Option<String>, modifiers: u32 },
    Back,
    Forward,
    Reload,
    /// Windowless browsers do not get focus implicitly; without this a click
    /// into a text field never gives it a caret and typing goes nowhere.
    Focus(bool),
    Url(String),
}

/// Shared between the CEF paint callback and the main loop.
struct Shared {
    /// Logical size CEF lays out at; read via `view_rect`.
    size: RefCell<(f32, f32)>,
    /// Address changes, so jim's URL bar reflects real navigation (link
    /// clicks, redirects) rather than only what we asked for.
    urls: RefCell<Vec<String>>,
    /// Device pixel ratio reported to CEF via `screen_info`.
    scale: f32,
    /// Surface we publish; recreated when the size changes.
    surface: RefCell<Option<SharedSurface>>,
    /// Frames to announce to jim.
    tx: Sender<Frame>,
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let socket_path = argv.next().unwrap_or_default();
    let url = argv.next().unwrap_or_else(|| "about:blank".into());
    let width: f32 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(900.0);
    let height: f32 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(620.0);
    // Retina: without this CEF assumes scale 1.0, paints logical-sized frames,
    // and the pane draws them at half size.
    let scale: f32 = argv.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);

    if socket_path.is_empty() {
        eprintln!("usage: jim-webview-host <socket> <url> [w] [h]");
        std::process::exit(2);
    }

    // macOS: load the framework from the enclosing bundle before any CEF call.
    #[cfg(target_os = "macos")]
    let _loader = {
        let exe = std::env::current_exe().expect("current_exe");
        let loader = library_loader::LibraryLoader::new(&exe, false);
        assert!(loader.load(), "could not load Chromium Embedded Framework");
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let mut app = AppBuilder::build(HostApp {});
    let ret = execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
    assert_eq!(ret, -1, "browser process expected");

    // Chromium enforces a process singleton on its cache directory, so two
    // hosts sharing the default path means the second `initialize` fails and
    // the second pane never renders. One directory per host.
    let cache_dir = std::env::temp_dir().join(format!("jim-webview-cache-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&cache_dir);

    let settings = Settings {
        root_cache_path: cache_dir.to_string_lossy().as_ref().into(),
        windowless_rendering_enabled: 1,
        // We own this process's loop, so we pump CEF ourselves and can
        // service the socket in the same loop.
        external_message_pump: 1,
        no_sandbox: 1,
        browser_subprocess_path: helper_path()
            .map(|p| p.to_string_lossy().as_ref().into())
            .unwrap_or_default(),
        ..Default::default()
    };
    assert_eq!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut()
        ),
        1,
        "cef::initialize failed"
    );

    let stream = UnixStream::connect(&socket_path).expect("connect to jim");
    // Deliberately BLOCKING. The reader runs on its own thread and blocking is
    // what we want there; making it non-blocking meant `lines()` returned
    // WouldBlock immediately, the command reader died on the first poll, and
    // every resize/scroll jim sent was silently dropped.
    let mut writer = stream.try_clone().expect("clone socket");

    let (tx, rx): (Sender<Frame>, Receiver<Frame>) = mpsc::channel();
    let shared = Rc::new(Shared {
        size: RefCell::new((width, height)),
        urls: RefCell::new(Vec::new()),
        scale,
        surface: RefCell::new(None),
        tx,
    });

    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        ..Default::default()
    };
    let browser_settings = BrowserSettings {
        windowless_frame_rate: 60,
        ..Default::default()
    };
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut ClientBuilder::build(
            RenderHandlerBuilder::build(HostRenderHandler {
                shared: shared.clone(),
            }),
            DisplayHandlerBuilder::build(HostDisplayHandler {
                shared: shared.clone(),
            }),
        )),
        Some(&url.as_str().into()),
        Some(&browser_settings),
        None,
        None,
    )
    .expect("create browser");

    eprintln!("[host] cef up, browser at {url}");

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[host] socket read error: {e}");
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(c) => {
                    if cmd_tx.send(c).is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("[host] bad command {line:?}: {e}"),
            }
        }
        // EOF: jim closed the socket, i.e. the GUI exited or was restarted.
        // Without this the host (and its Chromium helpers) is orphaned to
        // PID 1 and survives every dev-restart, piling up until something is
        // manually killed.
        eprintln!("[host] jim disconnected; exiting");
        std::process::exit(0);
    });

    loop {
        do_message_loop_work();

        while let Ok(cmd) = cmd_rx.try_recv() {
            // Loud on purpose while the input path is being brought up: it is
            // the only way to tell "jim never sent it" from "CEF ignored it".
            if !matches!(cmd, Command::Mouse { ref kind, .. } if kind == "move") {
                eprintln!("[host] cmd {cmd:?}");
            }
            apply(&browser, &shared, cmd);
        }

        {
            let pending: Vec<String> = shared.urls.borrow_mut().drain(..).collect();
            for url in pending {
                let msg = serde_json::to_string(&UrlMsg { url }).unwrap();
                if writeln!(writer, "{msg}").is_err() {
                    cef::shutdown();
                    return;
                }
            }
        }

        while let Ok(frame) = rx.try_recv() {
            let msg = serde_json::to_string(&FrameMsg { frame }).unwrap();
            if writeln!(writer, "{msg}").is_err() {
                eprintln!("[host] jim went away; exiting");
                cef::shutdown();
                return;
            }
            let _ = writer.flush();
        }

        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn helper_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?.to_path_buf();
    let p = contents.join("Frameworks/Jim Helper.app/Contents/MacOS/Jim Helper");
    p.exists().then_some(p)
}

fn apply(browser: &Browser, shared: &Rc<Shared>, cmd: Command) {
    let Some(mut host) = browser.host() else { return };
    match cmd {
        Command::Resize { w, h } => {
            *shared.size.borrow_mut() = (w.max(1.0), h.max(1.0));
            host.was_resized();
        }
        Command::Mouse { x, y, kind } => {
            let ev = MouseEvent {
                x: x as i32,
                y: y as i32,
                modifiers: 0,
                ..Default::default()
            };
            match kind.as_str() {
                "down" => host.send_mouse_click_event(
                    Some(&ev),
                    MouseButtonType::default(),
                    0,
                    1,
                ),
                "up" => host.send_mouse_click_event(
                    Some(&ev),
                    MouseButtonType::default(),
                    1,
                    1,
                ),
                _ => host.send_mouse_move_event(Some(&ev), 0),
            }
        }
        Command::Wheel { x, y, dx, dy } => {
            let ev = MouseEvent {
                x: x as i32,
                y: y as i32,
                modifiers: 0,
                ..Default::default()
            };
            host.send_mouse_wheel_event(Some(&ev), dx as i32, dy as i32);
        }
        Command::Key {
            kind,
            code,
            text,
            modifiers,
        } => {
            // CEF wants RAWKEYDOWN (so the page sees keydown handlers), then
            // CHAR (which is what actually inserts text), then KEYUP.
            let mut ev = KeyEvent {
                modifiers,
                windows_key_code: code,
                native_key_code: 0,
                is_system_key: 0,
                character: 0,
                unmodified_character: 0,
                focus_on_editable_field: 1,
                ..Default::default()
            };
            match kind.as_str() {
                "down" => {
                    ev.type_ = KeyEventType::from(sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN);
                    host.send_key_event(Some(&ev));
                }
                "up" => {
                    ev.type_ = KeyEventType::from(sys::cef_key_event_type_t::KEYEVENT_KEYUP);
                    host.send_key_event(Some(&ev));
                }
                "char" => {
                    if let Some(txt) = text {
                        for u in txt.encode_utf16() {
                            ev.type_ =
                                KeyEventType::from(sys::cef_key_event_type_t::KEYEVENT_CHAR);
                            ev.character = u;
                            ev.unmodified_character = u;
                            host.send_key_event(Some(&ev));
                        }
                    }
                }
                other => eprintln!("[host] unknown key kind {other:?}"),
            }
        }
        Command::Focus(on) => host.set_focus(if on { 1 } else { 0 }),
        Command::Back => {
            if browser.can_go_back() == 1 {
                browser.go_back();
            }
        }
        Command::Forward => {
            if browser.can_go_forward() == 1 {
                browser.go_forward();
            }
        }
        Command::Reload => browser.reload(),
        Command::Url(u) => {
            if let Some(mut frame) = browser.main_frame() {
                frame.load_url(Some(&u.as_str().into()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CEF handlers
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct HostApp {}

wrap_app! {
    struct AppBuilder {
        app: HostApp,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            if let Some(cl) = command_line {
                cl.append_switch(Some(&"off-screen-rendering-enabled".into()));
                // Chromium otherwise asks the macOS Keychain for its
                // "safe storage" key on every launch, which pops a permission
                // dialog each restart. The mock keychain keeps cookies and
                // passwords in-process instead — appropriate here, since each
                // pane is a throwaway browser with its own cache dir.
                cl.append_switch(Some(&"use-mock-keychain".into()));
            }
        }
    }
}

impl AppBuilder {
    fn build(app: HostApp) -> App {
        Self::new(app)
    }
}

#[derive(Clone)]
struct HostRenderHandler {
    shared: Rc<Shared>,
}

wrap_render_handler! {
    struct RenderHandlerBuilder {
        handler: HostRenderHandler,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let (w, h) = *self.handler.shared.size.borrow();
                eprintln!("[host] view_rect -> {w}x{h}");
                if w > 0.0 && h > 0.0 {
                    rect.width = w as _;
                    rect.height = h as _;
                }
            }
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            if let Some(info) = screen_info {
                // CEF multiplies the logical view rect by this to decide the
                // pixel size it paints, which is what makes retina crisp
                // instead of half-size.
                info.device_scale_factor = self.handler.shared.scale;
                return 1;
            }
            0
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            _type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (w, h) = (width as u32, height as u32);
            let len = (w as usize) * (h as usize) * 4;
            // SAFETY: CEF guarantees the buffer for the callback's duration.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };

            let shared = &self.handler.shared;
            let mut slot = shared.surface.borrow_mut();
            let needs_new = match slot.as_ref() {
                Some(s) => s.width != w || s.height != h,
                None => true,
            };
            if needs_new {
                match SharedSurface::new(w, h) {
                    Some(s) => {
                        eprintln!("[host] new IOSurface {} ({w}x{h})", s.id());
                        *slot = Some(s);
                    }
                    None => {
                        eprintln!("[host] could not create a {w}x{h} IOSurface");
                        return;
                    }
                }
            }
            let Some(s) = slot.as_ref() else { return };
            s.write_bgra(src, w, h);
            let _ = shared.tx.send(Frame { id: s.id(), w, h });
        }
    }
}

impl RenderHandlerBuilder {
    fn build(handler: HostRenderHandler) -> RenderHandler {
        Self::new(handler)
    }
}

#[derive(Clone)]
struct HostDisplayHandler {
    shared: Rc<Shared>,
}

wrap_display_handler! {
    struct DisplayHandlerBuilder {
        handler: HostDisplayHandler,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            // `cef::Frame`, not our serde `Frame` — the names collide.
            _frame: Option<&mut cef::Frame>,
            url: Option<&CefString>,
        ) {
            if let Some(url) = url {
                self.handler.shared.urls.borrow_mut().push(url.to_string());
            }
        }
    }
}

impl DisplayHandlerBuilder {
    fn build(handler: HostDisplayHandler) -> DisplayHandler {
        Self::new(handler)
    }
}

wrap_client! {
    struct ClientBuilder {
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }
    }
}

impl ClientBuilder {
    fn build(render_handler: RenderHandler, display_handler: DisplayHandler) -> Client {
        Self::new(render_handler, display_handler)
    }
}
