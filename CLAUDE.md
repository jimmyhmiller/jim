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
  Hosts the canvas, project-prism "cube", radial menu, projects
  (+ sidebar **workspaces**: saved sidebar configurations, swiped
  between with two fingers over the sidebar — every project exists in
  every workspace, a workspace only remembers which are parked and
  which one you were last in; `jimctl workspace`),
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

## Emacs panes

Two pane kinds, both in `crates/jim-emacs`:

- `"emacs"` (`src/lib.rs`) — the fallback: a tty frame from
  `emacsclient -t` on a shared `emacs --daemon=jim`, rendered through
  the jim-terminal grid.
- `"emacs-native"` (`src/native.rs`) — **the real one.** A forked GNU
  Emacs (`~/Documents/Code/emacs-jim`, the `jim` window system, whose
  port is written in Coil) serializes its own redisplay as draw-ops over
  a unix socket; jim replays them into a per-pane RGBA framebuffer,
  rasterising Emacs's glyph ids from Emacs's own font file. Emacs owns
  every pixel *position*, jim owns every *pixel*.

The GUI feel on top of that transport is a **duplex control channel**
(`~/.jim/emacs-ctl.sock`, newline-delimited text; jim is the server,
Emacs connects) plus `crates/jim-emacs/elisp/jim-integration.el`:

| direction | message | what it does |
| --- | --- | --- |
| jim → emacs | `theme <json>` | jim's whole design-token palette → the Emacs face set (syntax, region, mode line, dividers, line numbers). Re-sent on every theme change. |
| jim → emacs | `scroll <fid> <x> <y> <dy>` | trackpad PIXELS → `pixel-scroll-precision-scroll-*`. This is what makes scrolling smooth instead of 2-line notches. Consecutive same-frame scrolls are coalesced in the filter — see below. |
| jim → emacs | `click <fid> <x> <y> <n>` | double/triple-click word/line selection. Emacs cannot do this itself: the port's input record has no timestamp, so `make_lispy_event` never promotes a click. |
| jim → emacs | `focus <fid>` | Emacs's selected frame follows jim's focused pane. |
| jim → emacs | `font-px <px>` | default font size in PIXELS, from the `font_size` token. Emacs text is sized like the rest of jim's UI; a point size would land a third too big, since the port reports 96dpi. |
| jim → emacs | `open`/`font`/`cmd`/`eval` | visit a file, set the font size in points, run an interactive command, escape hatch. |
| emacs → jim | `state <fid> <json>` | buffer, file, modified, mode, point, and the viewport's top/bottom fraction. Drives the pane's scroll indicator and the `emacs.state` bus topic. |

`jim-integration.el` is loaded with `-l` at launch (written to
`~/.jim/emacs/` from `include_str!`), **not** added to the fork's
`lisp/term/jim-win.el` — that file is preloaded into the dump, so
changing it costs a re-dump, while this reloads on the next pane.

Syntax highlighting comes from tree-sitter: Emacs 30 ships `rust-ts-mode`
and friends but wires none of them into `auto-mode-alist`, so a `.rs`
file lands in Fundamental mode with no colour at all. `jim--setup-syntax`
opts in per language, but ONLY where the grammar is installed
(`M-x treesit-install-language-grammar`, which writes to
`~/.emacs.d/tree-sitter/`), so a missing grammar changes nothing rather
than erroring. It also raises `treesit-font-lock-level` to 4, because
jim has a token for variables/properties/operators/brackets and level 3
leaves all four the plain foreground colour.

**Scrolling up costs about 5× scrolling down, and that is Emacs, not us.**
Measured on a 2200-line buffer, 30px a step: down 1.4ms, up 6.6ms.
`pixel-scroll-precision-scroll-up` has to call `window-text-pixel-size`
with a negative offset from `window-start` — laying text out backwards —
where scrolling down just walks forward from a position it already has.
Nothing on the jim side changes that. What jim CAN do is not queue up
behind it: `jim--coalesce-scrolls` merges each run of consecutive
same-frame `scroll` lines in one filter chunk into a single larger
scroll. It is self-regulating (when Emacs keeps up, a chunk holds one
line and nothing changes) and it wins twice, because one 300px scroll is
one backwards layout rather than ten: a ten-deep backlog retires in
0.28s instead of 1.36s. Do NOT "improve" this with an idle timer —
a continuous gesture never lets Emacs go idle, so the accumulator would
never flush and scrolling would freeze until you let go.

`jim--setup` turns `window-divider-mode` OFF. jim-win.el enables it with
8px dividers for "GUI-style splits", but a split in jim is a *pane*
(`C-x 2`/`C-x 3` make a frame that jim docks), so it rarely divides
anything — and it does not survive Emacs's scroll optimization: the 8px
bottom divider gets drawn once, a later `scroll_run` blits it up into the
middle of the buffer, and nothing repaints that strip. The symptom is a
solid `chrome_divider`-coloured band straight through a line of code.

Two things to know when touching `jim-integration.el`: **byte-compiling
it is not enough to know it loads** — verify with
`emacs --batch -Q --eval '(load "…/jim-integration.el" nil t t)'`, since a
form that byte-compiles can still fail at load (an unescaped quote in a
docstring cost an hour here), and a load failure is SILENT in a pane: the
theme, scrolling, and state reporting just never turn on. And the port's
`defined_color` only understands `#rrggbb` — named colours like `grey85`
fail to load, which is why the generated theme is hex throughout.

**How to open one.** `emacs.workspace` — the "Emacs" action, ⌘K E, or
the radial ring — spawns a file tree docked beside a native Emacs pane,
rooted at the project's `default_cwd`. Agents and scripts get the same
thing from `jimctl emacs [--project P] [--path DIR]`; both go through
`open_emacs_workspace` in jim-app so there is one implementation. The
bare `emacs-native` kind is deliberately kept out of the radial and named
"Emacs Pane (no sidebar)" — an editor with no navigation beside it is
rarely what anyone means, but the kind must stay registered because
layout restore and Emacs-initiated splits spawn through it.

Bus topics: widgets emit `emacs.open_file` `{path}` and `emacs.command`
`{command}`, routed to the emacs pane docked with the sender (else the
focused one); jim publishes `emacs.state` (retained, per project).
`file_open.ft` is both ends of that — the "Emacs" action
(`emacs.workspace`, ⌘K E) spawns it docked beside an emacs pane.

Two things about `sync_emacs_frames`:

- **Never present a batch that has no `flush`.** Ops go into the CPU `fb`
  and are copied to the GPU image only on `flush`, precisely so a
  half-finished redisplay never reaches the screen. Presenting early
  (e.g. to chase a pane that looks stale) shows the old and new text
  overlaid — glyphs colliding, letters doubled. Tried it, reverted it.
  If a pane looks stale, the bug is upstream: ops not arriving, or the
  main loop not waking — not the flush gate.
- **OPEN BUG: the last text row still draws over the mode line.**
  Rounding the FRAME height to a whole number of rows is not enough, and
  the fit assertion in `sync_native_resize` confirms the frame height is
  already an exact multiple — the artifact persists anyway. The mode line
  and echo area are not necessarily one text-row tall, so a whole frame
  height does not imply a whole TEXT AREA. Root cause is that
  `draw_glyph_string` in the Coil port carries no clip rect: real Emacs
  relies on the window system to clip a partially-visible last row, and
  this port has nothing doing that, so the row is blitted whole and the
  mode line only covers its bottom half. The durable fix is to clip runs
  to the window's text area — either send the text-area bottom in the op
  stream (`window_box` has it) or add a clip rect to the run op. Do NOT
  try to fix this by rounding sizes; that has now failed twice.

- **OPEN BUG: the bottom rows of TERMINAL panes go stale.** Separate
  subsystem (`jim-terminal`'s `sync_grid`, not jim-emacs). Old cell
  content stays on screen interleaved with new until the pane is
  resized. Suspect the dirty-row bookkeeping around
  `local_dirty_rows` / `force_all` in `sync_grid`.

- **The frame's line height comes from a RUN's height, not from the font
  op.** `asc + desc` omits line-spacing (a 14px font reports ascent 13 +
  descent 3, while rows are 22px), and rounding the frame to the wrong
  multiple is exactly as good as not rounding — you get a partial bottom
  row drawing over the mode line, since the port has no clip rect.
  Learning it late also means re-sending the fit: `sync_native_resize`
  memoizes what it sent, so `resize_dirty` drops that memo. Learn it
  ONCE per font — runs are not all the same height (a smaller face, the
  echo area), and letting it change per batch re-fits the frame
  constantly, a resize storm the pane never settles out of.

Gotcha worth keeping: **never send a frame a resize before its
create-frame has been written.** `store-event` in the port falls back to
the *last* frame for an unknown id, so an early resize silently resizes
some other pane's frame — and since `sync_native_resize` memoizes what
it sent, the real frame then never gets sized at all and its pane stays
blank forever. `sync_native_resize` skips panes still in
`pending_create` for exactly this reason.

**Bold and italic** work by way of a font registry in the port: each
distinct `struct font *` is announced as `font … id=N wt= sl= path=…`
and every `run` names the id that drew it. The path alone is not enough —
Menlo.ttc holds regular, bold, italic and bold-italic behind one
filename — so `face_index_for` in native.rs picks the face whose swash
attributes match Emacs's numeric weight/slant (regular 80, bold 200).
There is a unit test for that selection.

**The caret** is a real bar, not a terminal block: the port emits
`cursor … kind=2` as a bare rect and jim fills it with the `caret` token,
so it tracks the theme live. `jim-cursor-type` in the elisp sets the
shape; `box` restores the old inverted-glyph block.

jim erases that caret itself, with a save-under (`restore_caret`).
**Do not assume Emacs erases a bar cursor.** A block cursor IS the
inverted glyph, so anything repainting the cell erases it; a bar is a
separate rect on top, and `display_and_set_cursor` erases by calling
`erase_phys_cursor` *directly* — never through the port's
`draw_window_cursor` hook — which erases only by redrawing the character
underneath, and skips even that on several paths (`goto mark_cursor_off`:
hpos past the end of the row, zero visible height, cursor in the fringe).
Those are the ghost carets that used to be left behind at a line's edge.
The restore is gated on the pixels still being caret-coloured, so when
Emacs *did* repaint we leave its work alone — restoring unconditionally
would drag an `hl-line` highlight along behind the caret.

Still missing in the port: `draw_glyph_string` passes no attribute flags,
so **underline / overline / strike-through do not render** (`backend.coil`
already reserves the `flags` bits for them). The Coil backend DOES build
against current Coil again — see JIM.md for the build, and note that
`coil build --lib` appends to an existing archive, so `rm -f
libjimbackend.a` first or a stale member silently wins the link.

Hover washes on list rows: a **selected** row gets no wash. It paints its
own background at `z + 0.001`, which is the exact depth the wash uses, and
two sprites at one depth z-fight — the row visibly flickers between the
two states while hovered. It still has to stay in `hover_washes` though:
`update_widget_hover` uses membership there to decide a row needs no
re-render on hover, so removing it would make hovering a selected row
re-render the pane and flash its text. Hence the `selected` flag on
`HoverWash` — in the list, but not painted.

## Dictation engines (⌘⇧M / ⌘⇧T)

`crates/jim-app/src/dictation/` has two engines behind one `Transcriber`
trait, switched at runtime by the palette ("Dictation: Use Phonon" / "Use
Whisper") or `~/.jim/dictation.json` (`{"engine": "phonon"}`), read when each
dictation starts. Both servers are shared services that outlive the GUI
(`server.rs`; records in `~/.jim/{whisper,phonon}-server`) — like
`jim-daemon`, don't kill them on restart.

- **Phonon-2** (`phonon.rs`): `phonon serve` from the pinned venv
  `scripts/install-phonon.sh` builds in `~/.jim/phonon/venv`, forced onto the
  CPU engine (`FERMION_DEVICE=cpu`). Streams over its WebSocket
  `/v1/audio/stream`. Never MLX: it grew to 9 GB and never released it.
  The script is compiled in (`include_str!`): jim runs it in the background
  whenever Phonon is selected and `~/.jim/phonon/installed` doesn't match the
  script's `PIN=` line, so bumping `PIN` reinstalls everywhere. Log:
  `~/.jim/phonon/install.log`.
- **Whisper** (`whisper.rs`): LocalAgreement over short windows. Gotchas,
  all measured: the prompt must hold only committed text whose audio has
  LEFT the window (whisper skips what it's prompted with); `verbose_json`
  token timestamps cost +0.3–0.7 s a pass and drift too much to diff on —
  use `srt` and edit-distance alignment; `audio_ctx` breaks turbo (12 s
  passes of garbage); `temperature_inc=0` keeps a repetition loop from
  running a pass into the timeout.

## Docked panes have a SLIM header, not `TITLE_H`

Anything mapping a cursor position into a pane's content space must use
the pane's actual title height — `jim_pane::override_title_h(chrome_ov)`
with `pt_to_content_local_th`, not the `TITLE_H`-assuming
`pt_to_content_local`. A docked pane's content starts higher up, so
assuming the full height shifts every hit-test down by the difference:
in a docked file tree that is almost exactly one row, and hovering a row
highlights the one above it. Presses are unaffected because
`PaneContentPressed` already carries an override-aware `local_pt` —
which is why clicking can be right while hovering is wrong, and why this
is easy to misread as a rendering bug.

The nastier half of this: `apply_widget_scroll` also placed
`content_root` with a hardcoded `TITLE_H`, while
`sync_chrome_override_geometry` places it by the override. Two systems
writing the same transform with different answers, so a docked widget's
content sat at one of two offsets depending on which ran last — hover was
intermittently a row off, and "fixing" the hit-test alone just moved
which half was wrong. Both now use `override_title_h`; so does the
content-box height in the clip walk.

Fixed in `update_widget_hover`, and in `context_menu.rs` (its widget
row-menu hit-test, which the docked file tree depends on — the pane there
is already at Bevy's 16-parameter ceiling, so the chrome override rides
along in the `panes` query rather than in one of its own). The same
assumption is still present in `jim-widget/src/lib.rs` at the popover
origins (lines ~970, ~1658, ~2172), `update_tooltip_hover` (~2126), the
two `pt_to_content_local` uses around ~2672/~2750, and
`glaze_material.rs` (~371) — all latent for docked panes.

A **docked member** pane also used to take `Undock`/`Close` for the whole
of its surface, before any widget row menu could be considered — which
would have made per-row menus impossible in exactly the panes that want
them most. A row carrying a `ListItem.context` now wins; Undock stays
reachable from the header and from empty space in the list.

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
