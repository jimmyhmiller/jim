# Channels — the agent bus

Bidirectional messaging between **Claude Code sessions** and the **jim
editor** (and, through the same fabric, between sessions, and between
sessions and funct widgets).

The design principle is **one fabric, everyone's a participant**. We do
*not* build a Claude-specific pipe. A Claude session becomes just another
participant on the widget↔widget bus jim already has — addressed by an
id, exchanging `{topic, payload, sender}` messages. Every capability
falls out of that one mechanism:

| You want | It's just… |
|---|---|
| editor → message a session | emit `agent.inbox.<id>` |
| session → message another session | emit `agent.inbox.<other>` |
| session ↔ funct widget | both emit/subscribe topics (`on_message`) |
| session → act on the editor | emit `jim.action` → `dispatch_local()` |
| editor → broadcast to all sessions | emit `agent.all` |

Participants and their `sender` ids: funct/subprocess widgets (their
widget id), the CLI (`jimctl msg`), the app itself, and **Claude
sessions** (`claude:<id>`). Same verbs in both directions — that is the
open part.

## What a channel is

A *channel* is an MCP stdio server that Claude Code spawns **per
session** (see `docs`: <https://code.claude.com/docs/en/channels.md>).
Requires Claude Code v2.1.80+ (v2.1.81+ for permission relay) and is a
research preview behind an allowlist; custom channels load with
`--dangerously-load-development-channels`.

Our bridge is **`jimctl channel`** — jimctl is already dylib-free and
already duplicates jim's socket wire formats, so the bridge gets
`~/.jim/socket` plumbing for free and `.mcp.json` simply runs
`jimctl channel`.

The hub is the standalone **`jim_bus` daemon** (`~/.jim/bus.sock`), *not*
the GUI — so the bus works whether or not the editor is open, the same way
the terminal works whether or not the GUI is open (via `jim_daemon`). Any
client that finds the bus down spawns it on demand (`<exe> bus-daemon`).
The GUI is just another client: it subscribes to deliver messages to
widget panes and publishes their emits.

```
   Claude session A          jim_bus daemon (the hub)         jim GUI (a client)
  ┌───────────────┐         ┌────────────────────────┐      ┌──────────────────┐
  │  claude        │         │  ~/.jim/bus.sock        │      │ subscribe ──────┐ │
  │   ▲  spawns    │  pub /  │   • retained store      │ pub/ │                 ▼ │
  │   │            │  sub    │     (persisted to disk) │ sub  │  WidgetMsgBus     │
  │ ┌─┴──────────┐ │◄───────►│   • resume ring         │◄────►│   │ on_message    │
  │ │jimctl      │ │         │   • agent roster +      │      │   ▼               │
  │ │  channel   │ │         │     dead-peer sweep     │      │  widgets          │
  │ │  (bridge)  │ │         └────────────────────────┘      │  jim.action →     │
  │ └────────────┘ │            ▲ persists                   │  dispatch_local   │
  └───────────────┘     ~/.jim/bus-retained.json             └──────────────────┘
   Session B runs its own `jimctl channel` → cross-session is just two
   clients on one daemon (and it keeps working with the GUI closed).
```

- **Inbound (bus → Claude):** the bridge subscribes to the daemon, filters
  to this session's subscriptions, and emits a
  `notifications/claude/channel` MCP notification. Claude wakes and reacts.
- **Outbound (Claude → bus):** Claude calls a tool; the bridge publishes a
  `Publish` frame (or a `jim.action` message) to the daemon. The GUI, if
  open, observes `jim.action` via its subscription and re-dispatches it
  through `dispatch_local()`.

Why this is nearly free on the app side: jim's widget bus **already has a
global, cross-project scope** (`PendingMsg.project: None`), is reachable
for *publish* via the `widget_message` IPC action, for *subscribe* via
the bus log, and the app already has `dispatch_local()` to run an
`IpcRequest` against itself.

## Enabling it (clean `--channels`, no dangerous flag)

The repo ships a **local-only** plugin + marketplace under `channel-plugin/`
— nothing is published; a Claude Code "marketplace" here is just a folder on
disk. One-time setup:

```bash
./scripts/channel-setup.sh
```

It registers the local marketplace `jim-local`, installs the `jim` plugin,
and (if needed) drops the `managed-settings.json` that allowlists it. Then
launch any session with:

```bash
claude --channels plugin:jim@jim-local      # alias: cj='claude --channels plugin:jim@jim-local'
```

The plugin's MCP config (`channel-plugin/jim/.mcp.json`) runs
`${HOME}/.cargo/bin/jimctl channel` with `JIM_CHANNEL_ID=main`.

Notes / why each piece:
- `--channels` only loads **plugins** on an allowlist — a bare `.mcp.json`
  `server:` entry can never load this way. So the channel must be a plugin
  in a marketplace (both local).
- The allowlist for a *custom* plugin comes from `allowedChannelPlugins` in
  **managed settings** (`/Library/Application Support/ClaudeCode/managed-settings.json`,
  admin/sudo — that's the one privileged step). Pro/Max individuals may have
  the allowlist waived: try the flag first, and only run the setup script's
  sudo step if the startup notice says the plugin isn't approved.
- `--channels` is per-session by design (no auto-load setting); use the `cj`
  alias.

**No-setup fallback:** `channel.mcp.json.example` + the dev flag still works
without any of the above:
`claude --dangerously-load-development-channels server:jim`.

## Addressing contract

Reserve an `agent.*` namespace on the **global** channel
(`project: null`) so it is cross-project:

| Topic | Payload | Meaning |
|---|---|---|
| `agent.hello.<id>` *(retained)* | `{id, label, cwd, project, pid, started}` | roster announce; tombstone on exit |
| `agent.inbox.<id>` | free JSON | direct message to one session |
| `agent.all` | free JSON | broadcast to every session |
| `agent.status.<id>` | `{state, note}` | a session reporting what it's doing |
| `jim.action` | `{action, params}` | editor command → `dispatch_local()` |
| *(any topic)* | — | a session may subscribe to `build.failed`, a widget's topic, etc. |

A session's **default subscriptions** are `agent.inbox.<id>` and
`agent.all`. Everything else is opt-in, so sessions aren't flooded.

Message payload convention (so any participant can render it):
`{ "from": "<sender id>", "text": "<human text>", "data": <optional JSON> }`.
`text` is what gets shown to Claude; `data` is structured cargo.

## MCP tool surface (north → south)

Few, generic, composable. New editor capabilities arrive as new `agent.*`
topics or new `IpcRequest` variants — **not** new protocol.

```
jim_send(to, text, data?)      to = "agent:<id>" | "all" | "topic:<name>"
jim_subscribe(topic)           start receiving a topic
jim_unsubscribe(topic)
jim_roster()                   live sessions + widgets (agent.hello.* + ListProjects)
jim_identify(label)            set this session's friendly name
jim_do(action, params)         generic passthrough to the IpcRequest surface
```

`jim_do` keeps it open: it maps onto the existing `IpcRequest` enum
(`open_file`, `spawn_widget`, `suggest_pane`, `screenshot`, `add_issue`,
`open_palette`, `activate_project`, `close_panes`, …). Add an
`IpcRequest` variant and Claude can use it with no new tool.

## Inbound shape

```
<channel source="jim" topic="agent.inbox.sess_123" sender="claude_456" project="Recursion">
  Hey, can you take over the auth refactor? Context: …
</channel>
```

`meta` carries `{topic, sender, project, kind}`. MCP meta keys must be
identifiers (letters/digits/underscore — hyphens are silently dropped),
so keep them snake_case; values are strings.

## Session identity

The bridge self-assigns `id` at startup (env `JIM_CHANNEL_ID`, else
`sess_<pid>`) and announces `agent.hello.<id>`. Because **claude-bus**
already tags every hook event with `claude_pid`, the editor can correlate
an agent-bus participant with that session's live hook stream
(pre_tool_use/stop/…) by pid — a roster widget can show both "session X
exists" and "session X is running Bash right now".

> Open question: whether Claude Code exposes a stable session-id env var
> to MCP subprocesses. If so, adopt it for stability across restarts
> instead of a random/pid id.

## What the app needs (small)

The new pieces:

1. **`jimctl channel`** — the bridge: a newline-delimited JSON-RPC-2.0
   stdio loop + a bus-log tail thread. No MCP crate needed; the surface
   is one notification out and a handful of tools in.
1b. **Two small `jim-app` extensions** (done in Phase 1): the
   `widget_message` IPC action gained a `"global"`/`"*"` project target
   (→ the `None` channel) and an optional `sender` field; and the bus
   `pump_widget_messages` now delivers `None`-project (global) messages to
   *every* widget, not just project-less ones. Together these make the
   `agent.*` bus genuinely global and preserve session identity (so
   reply-by-sender works instead of everything reading as `tbmsg`).
2. **`jim.action` bus consumer** (done in Phase 2) — the bus pump emits a
   generic `BusMessageObserved` Bevy message for every delivered message;
   a jim-app system (`dispatch_bus_actions`) reads the `jim.action` ones,
   deserializes the payload as an `IpcRequest`, and re-injects it via the
   existing `dispatch_local()`. Editor commands are now just a bus message
   like everything else, so a Claude session (`jim_do`) AND any funct
   widget (`emit("jim.action", …)`) drive the editor through one path.
3. **`claude-sessions.ft`** — a funct viewer widget (no Rust): lists
   `agent.hello.*`, lets you message a session (`emit agent.inbox.<id>`),
   shows `agent.status.*`. A pure bus participant — it proves the symmetry.

## Phases

- **Phase 0 — DONE:** `jimctl channel` with inbound (tail → channel
  notif) + `jim_send`. Launch with
  `claude --dangerously-load-development-channels server:jim`.
- **Phase 1 — DONE:** roster (`agent.hello` + `jim_identify`/`jim_roster`),
  `jim_subscribe`/`jim_unsubscribe`, exit tombstone, the global-channel +
  `sender` app extensions, and the `claude-sessions.ft` viewer widget.
  Cross-session messaging + reply-by-sender verified.
- **Phase 2 — DONE:** `BusMessageObserved` pump message + the
  `dispatch_bus_actions` consumer + the `jim_do` tool. Verified: `jim_do
  add_issue` published `jim.action`, the app logged `[jim.action]
  add_issue`, and the issue landed. Claude (and any widget) can drive the
  editor's full `IpcRequest` surface.
- **Phase 3:** permission relay — **intentionally skipped.** This project
  always runs Claude Code in bypass-permissions mode, so no tool-approval
  prompts ever fire; an approve/deny relay would be dead weight. Remaining
  Phase 3 work is just launch ergonomics: package the channel as a plugin
  so it's `--channels plugin:jim@…` instead of the dev flag.

## Decisions / risks

- **Subscribe transport — RESOLVED.** Originally a `widget-bus.log` tail
  (200ms poll, GUI-written). Now a real socket subscribe with
  retained-replay + live streaming against the `jim_bus` daemon
  (`jim_bus::client::BusHandle`), so there's no file-tail latency and no
  GUI dependency.
- **Hub availability — RESOLVED.** The hub used to be the GUI, so
  session↔session needed the editor running. The bus now lives in the
  standalone `jim_bus` daemon (the same daemon pattern as `jim_daemon`),
  spawned on demand, persisted to disk — session↔session works with the
  GUI closed.
- **Trust.** Once `jim.action` can drive `dispatch_local`, any bus
  participant can command the editor. Locally that's the feature; keep a
  conscious "the bus is trusted within one user's machine" stance. (This
  matters more here because we run bypass-permissions — there's no
  per-action approval backstop, by design.)

## Wire formats (so the bridge stays dylib-free)

The bridge / adapters talk to the `jim_bus` daemon via `jim_bus::client`
(length-prefixed bincode frames — `crates/jim-bus/src/proto.rs`):

- **Publish:** `Hello{Publisher}` then `Publish(BusMessage{ project:
  Option<u64>, topic, payload_json, sender, retain })`. `project: None` is
  the cross-project global channel the `agent.*` topics ride on; `sender`
  stamps the real origin. Payloads ride as a JSON **string** (bincode
  isn't self-describing, so it can't carry a `serde_json::Value`).
- **Subscribe:** `Hello{Subscriber{since_seq}}` then read `BusFrame::Message
  { seq, msg }` frames; the daemon brackets the retained replay with
  `ReplayStart`/`ReplayEnd`, then streams live.
- The legacy `widget_message` action on `~/.jim/socket` still works as a
  thin GUI→daemon forwarder for backward compatibility.
- **MCP:** newline-delimited JSON-RPC 2.0 on stdio. Channel capability is
  `capabilities.experimental["claude/channel"] = {}`; inbound events are
  `notifications/claude/channel` with `params {content, meta}`.
