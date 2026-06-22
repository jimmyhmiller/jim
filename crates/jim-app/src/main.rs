use bevy::prelude::*;
use jim_app::AppShellPlugin;

fn main() {
    // Self-exec daemon mode: when the editor needs a per-session daemon
    // it re-execs this same binary with `--daemon <session_id> <cmd...>`.
    // Dispatch before touching Bevy so the daemon process never loads
    // the GUI stack.
    let mut args = std::env::args().skip(1);
    let first = args.next();
    // Self-exec bus-daemon mode: whoever needs the agent/widget message
    // bus spawns `<exe> bus-daemon`. Like `--daemon`, dispatch before
    // touching Bevy so the daemon never loads the GUI stack. It
    // double-forks itself (daemonize_if_requested), so the spawner's
    // wait() returns promptly.
    if first.as_deref() == Some(jim_bus::DAEMON_ARG) {
        jim_bus::daemon::daemonize_if_requested();
        if let Err(e) = jim_bus::run() {
            eprintln!("[jim-bus] fatal: {e}");
            std::process::exit(1);
        }
        return;
    }
    if first.as_deref() == Some("--daemon") {
        let session_id: u64 = args
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                eprintln!(
                    "usage: terminal --daemon <session_id> <program> [args...]"
                );
                std::process::exit(2);
            });
        let command: Vec<String> = args.collect();
        if command.is_empty() {
            eprintln!("terminal --daemon: missing program to run");
            std::process::exit(2);
        }
        jim_daemon::daemon::run(session_id, command);
    }

    // Make a Finder/Dock/LaunchServices launch behave EXACTLY like a
    // terminal launch. A GUI launch inherits launchd's minimal
    // environment — none of the vars the user exports from their shell
    // rc files (`DEEPSEEK_KEY`, `JIMMY_API_KEY`, a real `PATH`, …) are
    // present, so e.g. the Actions agent's `LlmConfig::from_env` finds
    // no key. Resolve the login+interactive shell env once here, before
    // any thread spawns (the only point where `set_var` is sound), so
    // every downstream `std::env::var` sees the same world either way.
    // Skipped for `--daemon` (re-exec'd by the GUI; inherits its env).
    import_login_shell_env();

    eprintln!("[terminal-bevy] startup marker: bundle-identity-test");

    let mut app = App::new();
    // Register the per-project asset source for style-bevy BEFORE
    // DefaultPlugins, since AssetPlugin (part of DefaultPlugins)
    // freezes the source registry once it's added.
    if let Some(data_dir) = jim_app::data_dir() {
        jim_style::register_style_asset_source(&mut app, data_dir.join("projects"));
        jim_style::register_preset_asset_source(&mut app, data_dir.join("styles"));
    }
    // Restore the size+position the user left the window at last
    // run, if we recorded one. First run / missing-or-corrupt file →
    // hard-coded defaults.
    let saved = jim_app::window_geometry::load();
    let (init_w, init_h) = saved
        .map(|g| (g.w, g.h))
        .unwrap_or((1200, 760));
    let init_position = saved
        .map(|g| WindowPosition::At(IVec2::new(g.x, g.y)))
        .unwrap_or(WindowPosition::default());
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Jim".into(),
            resolution: (init_w, init_h).into(),
            position: init_position,
            ..default()
        }),
        ..default()
    }));
    // Stash the saved geometry so `fit_window_to_monitor` can re-apply it
    // once the OS scale factor is known (the window-creation path mis-
    // scales it — see window_geometry). Captured here, before any system
    // can overwrite window.json with the wrong creation-time size.
    app.insert_resource(jim_app::window_geometry::RestoredGeometry(saved));
    app.add_plugins(AppShellPlugin);
    // Subscribe to Claude Code hook events from the central bus. Any
    // system in this app (or its panes) can react by reading
    // MessageReader<claude_bus_bevy::ClaudeBusEvent>. If the bus isn't
    // running the subscriber thread just retries in the background —
    // nothing in the app blocks on it.
    app.add_plugins(claude_bus_bevy::BusEventPlugin::default());
    app.run();
}

/// Import the user's login + interactive shell environment into this
/// process so a Dock/Finder launch matches a terminal launch.
///
/// We run the login shell with `-l -i` so BOTH the login files
/// (`.zprofile`/`.zlogin`/`.profile`) and the interactive rc
/// (`.zshrc`/`.bashrc`) are sourced — users routinely `export` API keys
/// from the interactive rc, which a non-interactive shell would miss.
/// The shell dumps its env NUL-delimited (`env -0`) after a unique
/// marker, so values containing newlines survive and any banner the rc
/// prints before the marker is ignored.
///
/// Best-effort: on any failure we leave the inherited env untouched (a
/// terminal launch already has everything; a Dock launch just stays as
/// degraded as before). Every var the shell reports is set — including
/// `PATH` — so the match is exact, not a curated allowlist. Vars that
/// only exist in our GUI process (launchd/AppKit internals the shell
/// never defines) are left alone because they never appear in the dump.
///
/// Must be called before any thread spawns: `std::env::set_var` is only
/// sound while the process is single-threaded (hence the `unsafe`).
#[cfg(target_os = "macos")]
fn import_login_shell_env() {
    use std::process::Command;

    const MARKER: &[u8] = b"__JIM_ENV_BEGIN__";

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // `printf` the marker, then `env -0`. The marker terminates with a
    // NUL too so it parses as just another (skipped) record boundary.
    let script = "printf '__JIM_ENV_BEGIN__\\0'; env -0";
    let output = match Command::new(&shell)
        .args(["-l", "-i", "-c", script])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            eprintln!(
                "[shell-env] {} exited {}; keeping inherited env",
                shell, o.status
            );
            return;
        }
        Err(e) => {
            eprintln!("[shell-env] could not run {}: {}; keeping inherited env", shell, e);
            return;
        }
    };

    // Take everything after the marker so rc-file banner noise is dropped.
    let Some(start) = find_subslice(&output, MARKER) else {
        eprintln!("[shell-env] marker not found in shell output; keeping inherited env");
        return;
    };
    let body = &output[start + MARKER.len()..];

    let mut count = 0usize;
    for record in body.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        // Split on the first '='. Keys are ASCII; a value may be any
        // bytes, so keep it as the original slice and lossy-convert.
        let Some(eq) = record.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, val) = (&record[..eq], &record[eq + 1..]);
        if key.is_empty() {
            continue;
        }
        let key = String::from_utf8_lossy(key);
        let val = String::from_utf8_lossy(val);
        // SAFE: still single-threaded — called from `main` before
        // `App::new()` / any plugin spawns a thread.
        unsafe {
            std::env::set_var(key.as_ref(), val.as_ref());
        }
        count += 1;
    }
    eprintln!("[shell-env] imported {} vars from {}", count, shell);
}

/// No-op off macOS: the minimal-env-on-GUI-launch problem is
/// macOS/launchd-specific.
#[cfg(not(target_os = "macos"))]
fn import_login_shell_env() {}

/// First index of `needle` in `haystack`, or `None`.
#[cfg(target_os = "macos")]
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}
