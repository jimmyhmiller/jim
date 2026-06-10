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
  Rhai** scripts (`src/rhai_widget.rs`, worker thread + named handlers
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
- `crates/jim-style`, `crates/glaze` — per-project styling + the Glaze
  shader/style language. `crates/jim-diff`/`diff-core` — diff pane +
  model. `crates/jim-inference` — classifier prompts + `style-muse`.
  `crates/claude-bus*` + `claude-*` — Claude Code event bus & hook
  tools (kept plain; reusable outside Jim).
- `crates/jimctl` — the `jim`-control CLI multi-tool. One binary with
  subcommands (`jimctl open|widget|inbox|project|suggest|msg|close|
  issue|inject`), replacing the old `tb*` binaries. Deliberately
  lib-free of `jim-app` (no libghostty dylib / @rpath dance); only
  depends on the dylib-free `jim-daemon`.

The GUI's LaunchServices identity (`CFBundleIdentifier =
com.jimmyhmiller.terminal-bevy`) is FROZEN despite the rename — changing
it would lose the Dock pin. Same for the `TERMINAL_BEVY_*` runtime env
vars and the `/tmp/.terminal-bevy` socket dir.

## libghostty-vt patch

`Cargo.toml` pins `libghostty-vt` / `libghostty-vt-sys` to a git rev of
`Uzaaft/libghostty-rs` that includes the zig optimize-mode fix (upstream
`3378f0b`). Without it, vendored ghostty builds default to zig Debug,
which makes `vt_write` 100x+ slower. Crates.io 0.1.1 predates the fix,
so the patch has to stay until upstream cuts a new release.
