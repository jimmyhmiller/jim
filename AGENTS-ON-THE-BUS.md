# Putting other agents on the jim bus

The jim **agent bus** (see CHANNELS.md) is runtime-agnostic. "Channels" is
only *Claude Code's* push protocol; any other agent — Codex, pi, a custom
OpenAI-compatible loop — joins the **same bus** through a thin **adapter**.
This is the contract for writing one.

## The bus is the open API

- **Publish** — one JSON line to `~/.jim/socket`:
  `{"action":"widget_message","project":"global","topic":"<t>","payload":{…},"retain":<bool>,"sender":"<id>"}`
- **Subscribe** — tail `~/.jim/widget-bus.log` (NDJSON; filter by `topic`).
- Or just shell out: `jimctl msg emit --topic … --json …` and `jimctl msg tail`.
- **Topics**: `agent.hello.<id>` (retained roster; null payload = tombstone),
  `agent.inbox.<id>` (direct), `agent.all` (broadcast), `jim.action`
  (payload is an `IpcRequest` → drives the editor: open files, spawn
  widgets, …). Payload convention: `{ from, text, data? }`.

Any process that can write a socket line and tail a file is already a
first-class participant — **outbound + editor control work for everything
today, no new code.** The only runtime-specific part is inbound *push*.

### Addressing: point-to-point vs broadcast

Routing is purely which topic you publish to:
- `agent.inbox.<id>` — only the bridge with that id is subscribed →
  **point-to-point**. This is how you reach one specific agent, and how
  adapters send a **reply back to whoever asked** (`agent.inbox.<from>`).
- `agent.all` — every bridge is subscribed → **broadcast**.

The adapters reply point-to-point to the sender by default (a reply to a
question goes to the asker, not everyone). Broadcasting is still available
by publishing to `agent.all` directly.

### Identity: fixed id, changeable name

Each adapter has a **fixed id** (its bus address — stable across restarts:
`--id`, or `$JIM_<X>_ID`, else `<kind>-<dir>`) and a **display name** /
label (free to change). Don't share one id across two live sessions — they
collide on `agent.hello.<id>` / `agent.inbox.<id>`. Change the name anytime
with the `--name` flag or the live `/name <label>` command (Claude's
adapter uses the `jim_identify` tool for the same thing).

## The adapter contract — 3 jobs

1. **Announce.** On start, publish retained `agent.hello.<id>`
   `{id,pid,cwd,label}`; on exit, publish retained `null` (tombstone).
   Optionally sweep dead peers (`kill -0` on their `pid`).
2. **Inbound (bus → agent).** Tail the bus; for `agent.inbox.<id>` /
   `agent.all`, get the message into the agent's context. *This is the hard,
   runtime-specific part — push vs pull.*
3. **Outbound (agent → bus).** Give the agent a way to publish:
   `agent.inbox.<other>` (message a peer), `agent.all` (broadcast),
   `jim.action` (drive the editor). Usually exposed to the agent as a tool.

`jimctl channel` (`crates/jimctl/src/cmd_channel.rs`) is the reference
adapter — read it before writing another.

## Per-runtime status

### Claude Code — DONE
MCP channel: `notifications/claude/channel` inbound, MCP tools outbound.

### Codex (`codex-cli`) — REAL TUI, DONE (`jimctl codex`)
Bridges your **live, interactive `codex` session** (not a front-end).
Requires the standalone codex install (`curl -fsSL https://chatgpt.com/codex/install.sh | sh`).
Usage:
```bash
jimctl codex                  # start FIRST; id = codex-<dir>
jimctl codex --id codex-main --name "Backend Codex"
# then, in another terminal:
codex                         # plain — auto-attaches to the same daemon
```
Now bus messages to `agent.inbox.<id>` / `agent.all` appear as **real turns
in your live codex TUI**, and the model's reply routes **point-to-point back
to the asker** (`agent.inbox.<from>`). Runs the injected turn full-auto
(`approvalPolicy:"never"`). Code in `crates/jimctl/src/cmd_codex.rs`.

How it works (verified against codex 0.139): `jimctl codex` runs `codex
app-server daemon start` to expose the shared control socket
(`~/.codex/app-server-control/app-server-control.sock`), then connects to it
as a **WebSocket-over-unix** client (raw JSON is dropped — it's WS frames,
JSON-RPC-lite with no `jsonrpc` field, and rejects an `Origin` header). A
plain `codex` auto-attaches to that daemon. The adapter:
- discovers the TUI's live thread from the global `thread/started` broadcast;
- injects with `turn/start{threadId,input:[{type:"text",text}],approvalPolicy:"never"}`;
- since the connection that *starts* a turn is excluded from that turn's
  event stream, it reads the reply by polling `thread/resume{threadId,
  itemsView:"full",limit:3}` once the turn finishes (the thread persists) and
  pulling the `agentMessage` out of `result.thread.turns[].items[]`;
- tracks busy/idle via the global `thread/status/changed`.

Caveats: experimental protocol (may shift between codex versions); needs the
daemon (`jimctl codex` starts it); `remote-control` is the WRONG path (it's
cloud/phone control with local transports off). Follow-up: give Codex the
`jim_send`/`jim_do` *tools* via `codex mcp` so it can initiate, not just reply.

### pi (`pi` coding CLI) — REAL TUI via extension (`integrations/pi/jim-bus.ts`)
**This is the real-session integration** — it runs inside your actual
interactive `pi`, not a separate front-end. Install:
```bash
cp integrations/pi/jim-bus.ts ~/.pi/agent/extensions/jim-bus.ts
```
Then just run `pi` normally — your session is on the bus as `pi-<dir>` (or
`$JIM_PI_ID`). It uses pi's extension API: `pi.sendUserMessage()` injects a
bus message as a real turn (inbound); `agent_end` routes the reply back to
the asker's inbox (point-to-point); `pi.registerTool()` gives the agent
`jim_send` / `jim_do` (deliberate outbound). `/jim-name <label>` renames
live, `/jim-who` shows identity. Verified: a bus ask appeared as a turn in
the live session and its reply went point-to-point to the asker. Only
locally-typed prompts stay off the bus.

A pi extension can inject into a running session because the API exposes
`sendUserMessage`/`sendMessage` (with `deliverAs: steer|followUp|nextTurn`)
— pi's docs even list "file watchers, webhooks, CI triggers" as intended
uses.

### pi headless worker — also available (`jimctl pi`)
A standalone delegate worker (no interactive session needed), same shape
and flags as `jimctl codex`:
```bash
jimctl pi                                     # id = pi-<dir>, name = <dir>
jimctl pi --id pi-main --name "pi (frontend)"
```
pi has no async-push protocol, but stable session resume (`--session-id`,
created if missing). Each ask — typed here or arriving on the bus —
continues **one persistent pi session** via `pi --mode json --print
--session-id jim-pi-<id> "<msg>"`; the final assistant text (from the
`agent_end` NDJSON event) goes **point-to-point to the asker's inbox** (or
prints here if you typed it). A worker thread serializes invocations so
concurrent asks don't race the session. Same `/name` `/who` `/quit`
controls and fixed-id/changeable-name identity as Codex. Code in
`crates/jimctl/src/cmd_pi.rs`.

Trade-off vs Codex: discrete per-message (process startup each turn) rather
than a live injected turn — but dead simple and uses only pi's documented
`--print`. Future upgrades: pi's `--mode rpc` (long-running session) to drop
per-message startup; a pi extension calling `jim_send`/`jim_do` for
deliberate outbound (not just replies).

### Custom OpenAI-compatible agent — easiest
You own the loop, so there's no protocol to reverse-engineer:
- subscribe to the bus (tail) and inject `agent.inbox.<id>` / `agent.all`
  messages as user/system turns;
- expose `jim_send` / `jim_do` as function-call tools that publish to the bus.
Full bidirectional in ~100 lines. When this exists, it's the template the
others approximate.

## Making it even easier (optional, not yet built)
Today an adapter hand-uses `jimctl msg emit`/`tail` or the raw socket. A thin
`jimctl agent {announce,send,recv,roster}` convenience layer would remove the
boilerplate (roster/tombstone/sweep, inbox-only NDJSON stream). Worth adding
when the first non-Claude adapter lands.
