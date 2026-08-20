# editor-idea

Experimental Bevy-based canvas of floating "panes" — each pane is a
draggable/resizable widget on an infinite-ish 2D surface. The canvas
hosts multiple widget kinds; right now: a **terminal emulator** (built
on `libghostty-vt`), a **text editor**, and a **run-button** widget.

When the user mentions "the terminal" in this directory, they almost
always mean `jim-terminal` (the terminal emulator we're building),
**not** the macOS terminal application or Claude Code's terminal UI.
Same for "the editor" → `jim-editor`. The whole app is **Jim** — the
GUI binary is `jim` (crate `jim-app`), config lives under `~/.jim`.

Crate naming: app-specific Bevy crates carry a `jim-` prefix; pure
model crates keep `-core`; generic/reusable crates (`glaze`,
`claude-bus`) stay plain. Package names use underscores (`jim_app`),
dirs use hyphens (`crates/jim-app`).

When changes need to be loaded into the running Jim application, always
build and restart it with `./scripts/dev-restart.sh`. Do not substitute a
plain `cargo build`, `cargo run`, direct binary launch, or manual app restart.
Wait for the script to confirm that Jim launched before reporting completion.

## Workspace layout

- `crates/editor-core` — buffer/selection/transaction/history/commands.
  Pure logic, no Bevy. The model layer for the editor pane.
- `crates/jim-pane` — shared chrome + lifecycle for floating panes
  (drag by title bar, corner resize, close button, focus, z-order,
  hit-testing, persistence, radial menu). New widget kinds register
  via `PaneRegistry` with a `PaneKindSpec`.
- `crates/jim-editor` — text-editor pane: renders spans into a pane's
  content_root, owns caret/selection visuals, scroll, keyboard input,
  syntax highlight. Provides `EditorPlugin` (standalone) and
  `EditorEmbedPlugin` (for hosts that already own camera/font).
- `crates/jim-widget` — retained-UI widget panes. Two hosting paths
  sharing one `Element` vocabulary (`src/protocol.rs`): **in-process
  funct** scripts (`src/script_widget.rs`, worker thread + named handlers
  like `on_click`/`on_toggle`/`on_input_change`/`on_bus`, hot reload from
  `~/.jim/widgets/`) and **subprocess** widgets (`src/lib.rs`,
  NDJSON `HostEvent`/`WidgetMsg` over stdio). UI events and the Claude
  Code bus are SEPARATE channels — `on_bus` is the bus, not UI. See
  `crates/jim-widget/AUTHORING.md` for the full handler/event model.
- `crates/jim-terminal` — terminal-emulator widget on top of
  `libghostty-vt`. Each terminal is an Entity; the `!Send` VT runtime
  lives in a `NonSend<TerminalStore>` keyed by entity. Per-cell
  textured sprites sample a shared `GlyphAtlas`. v0 has direct key
  encoding (no Kitty kb), no wide-char, no mouse reporting, no
  scrollback panning. Exposes `jim_terminal::TerminalPlugin` (the
  widget systems); the host installs `TerminalIdAllocator`/
  `TerminalInitialCwd`/`TerminalDirtyHook` closure-resources to wire in
  project policy without `jim-terminal` depending on the shell.
- `crates/jim-app` — the **Jim** application shell (binary `jim`).
  Hosts the canvas, project-prism "cube", radial menu, projects,
  suggestion drawer, inbox, command palette, IPC socket, and
  run-button infrastructure. `AppShellPlugin` adds
  `jim_terminal::TerminalPlugin` plus all shell plugins, and keeps the
  `Projects`/`Sidebar`-coupled glue (`handle_scroll`, bell/Claude
  notification pulses).
- `crates/jim-daemon` — per-session headless PTY daemon (binary
  `jim-daemon`); holds live shell state across GUI restarts. **Never
  kill these.** Runtime socket dir `/tmp/.terminal-bevy-<uid>` is
  FROZEN (legacy path; live daemons key on it).
- `crates/jim-bus` — the standalone widget↔widget / agent (`agent.*`)
  message-bus daemon. Same idea as `jim-daemon` but for messaging: it
  owns `~/.jim/bus.sock`, the persisted retained store
  (`~/.jim/bus-retained.json`) + agent roster, and the dead-peer sweep,
  so the bus survives a GUI restart. Dylib-free; both `jim` and `jimctl`
  host it by self-exec (`<exe> bus-daemon`) and connect as clients via
  `jim_bus::client`. The GUI is just another client now (subscribes to
  deliver to widgets, publishes their emits); the old `widget_message`
  action on `~/.jim/socket` is a thin GUI→daemon forwarder. See
  CHANNELS.md / AGENTS-ON-THE-BUS.md.
- `crates/jim-git` — repo-state snapshots (`compute_repo_state` →
  `RepoState`) + narrow debounced `.git` watching. Feeds the GUI's
  `git_watcher` plugin (retained bus topics `git.repo.<hash16>` global +
  `git.status` per-project) and `jimctl git` (queries, safe mutations,
  hunk-level stage/unstage à la `git add -p`). `crates/jim-review` —
  local code-review thread store (`~/.jim/reviews/<repo_hash>.json`),
  surfaced via `jimctl review` (+ `review.changed` bus events); agents
  read/reply with it. Widget suite: `git.ft` (shared lib) + `repo_hub` /
  `branches` / `stage` / `review_inbox` / `ai_work` / `pr_detail` +
  evolved `diff.ft` / `pr_dashboard.ft`; preset
  `scripts/github-workspace.sh`.
- `crates/jim-style`, `crates/glaze` — per-project styling + the Glaze
  shader/style language. `crates/jim-diff`/`diff-core` — diff pane +
  model. `crates/jim-inference` — classifier prompts + `style-muse`.
  `crates/claude-bus*` + `claude-*` — Claude Code event bus & hook
  tools (kept plain; reusable outside Jim).
- `crates/jim-webview` — web pane (kind `"webview"`), backed by Chromium.
  jim does **not** link CEF. `crates/jim-webview-host` is a separate binary
  that owns CEF and one browser; jim talks to it over a unix socket and gets
  frames as **IOSurface ids** (a u32 — pixels never cross the socket, they
  stay in GPU-shareable memory). `crates/jim-webview-helper` (`jim-helper`)
  is the tiny executable Chromium launches its renderer/GPU processes from;
  `make-bundle.sh` copies it into five `Jim Helper*.app` bundles.
  Out-of-process is NOT optional: in-process CEF crashes jim, because Bevy
  runs AppKit's `-[NSApplication run]` loop and Chromium's macOS message pump
  installs CFRunLoop observers that trap inside it (EXC_BREAKPOINT under
  `__CFRunLoopDoObservers`). Adding `CrAppProtocol` to `NSApplication` at
  runtime does not help.
  Servo was tried first and abandoned: it could not resize acceptably —
  270-666ms (occasionally ~10s) from a pane resize to a correctly sized
  frame, with a 100% blank white frame in between. CEF does the same resize
  in ~83ms and never emits a blank frame.

- `crates/jimctl` — the `jim`-control CLI multi-tool. One binary with
  subcommands (`jimctl open|widget|inbox|project|suggest|msg|close|
  issue|inject`), replacing the old `tb*` binaries. Deliberately
  lib-free of `jim-app` (no libghostty dylib / @rpath dance); only
  depends on the dylib-free `jim-daemon`.

The GUI's LaunchServices identity (`CFBundleIdentifier =
com.jimmyhmiller.terminal-bevy`) is FROZEN despite the rename — changing
it would lose the Dock pin. Same for the `TERMINAL_BEVY_*` runtime env
vars and the `/tmp/.terminal-bevy` socket dir.

## Chromium (CEF) webview gotchas

Learned the hard way; all of these fail silently or crash rather than
explaining themselves:

- **Helper bundle ids must all be `<main id>.helper`.** Chromium derives the
  Mach rendezvous service name by stripping ONE `.helper` suffix from the
  running bundle id. Per-type ids (`.helper.gpu`) break the lookup with
  `bootstrap_look_up …MachPortRendezvousServer.N: Unknown service name` and
  every renderer dies at startup.
- **Each host needs its own `root_cache_path`.** Chromium enforces a process
  singleton on the cache dir, so the second host's `cef::initialize` just
  fails and that pane never renders.
- **`screen_info` must report the device scale factor.** Without it CEF
  assumes 1.0, paints logical-sized frames, and the pane draws at half size.
- **The host's socket must stay blocking.** Non-blocking makes
  `BufReader::lines()` return `WouldBlock` immediately, the command reader
  exits on its first poll, and every resize/scroll jim sends is dropped.
- **Never put non-finite floats on the wire.** jim signals pointer-leave with
  `x = inf`; `serde_json` writes that as `null` and the host rejects the
  message.
- **The host must exit on socket EOF**, or it is orphaned to PID 1 on every
  `dev-restart` and its Chromium helpers accumulate.

## libghostty-vt patch (fork) + zig 0.16

`Cargo.toml` pins `libghostty-vt` / `libghostty-vt-sys` to our fork
`jimmyhmiller/libghostty-rs` (branch `ghostty-zig-0.16`). The fork is
`Uzaaft/libghostty-rs` at rev `d9dbd94` (which carries the zig
optimize-mode fix, upstream `3378f0b` — without it vendored ghostty
builds default to zig Debug and `vt_write` is 100x+ slower) plus ONE
change: it bumps the vendored `GHOSTTY_COMMIT` (in the sys crate's
`build.rs`) to a ghostty-master rev that requires **zig 0.16.0**.
ghostty went 0.15.2 → 0.16.0 on 2026-07-21; the VT C API (`vt.h`) is
unchanged, so the `d9dbd94` bindings compile/link against it as-is
(verified: full workspace build + 17/17 `vt_replay` runtime tests).

**Build requirement: zig 0.16.0 on PATH.** Local install lives at
`~/.local/zig-0.16.0` with `~/.local/bin/zig` symlinked to it
(`~/.local/zig-0.15.2` kept for rollback). CI uses `mlugg/setup-zig`
`0.16.0` on `macos-latest` (0.16 handles the macOS 26 SDK; 0.15.2 did
not, which is why CI was pinned to macos-15 before).

Retire the fork and return to a plain `Uzaaft/libghostty-rs` pin once
upstream bumps its own vendored ghostty past the zig-0.16 migration.
