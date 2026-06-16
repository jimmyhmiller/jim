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
in your live codex TUI**, framed with the sender, and the agent
**collaborates by choice via tools** — `jim_send` (reply to the asker, message
a peer, or `to:"all"` to broadcast), `jim_roster`, `jim_do`. Runs the injected
turn full-auto (`approvalPolicy:"never"`). Code in `crates/jimctl/src/cmd_codex.rs`.

**Inbound + outbound are split** (codex can't take an injected tool the way a
pi extension can):
- **Inbound** = the app-server injection below.
- **Outbound** = the `jim` MCP tool server (`jimctl mcp`). `jimctl codex`
  **auto-registers it** with codex on startup (`codex mcp add jim --env
  JIM_AGENT_ID=<id> -- jimctl mcp`), so the live session gets the tools. There
  is **no forced auto-reply** — the agent replies when it chooses, like pi.
  (Because registration writes codex config, start `jimctl codex` *before*
  launching `codex`.)

How inbound works (verified against codex 0.139): `jimctl codex` runs `codex
app-server daemon start` to expose the shared control socket
(`~/.codex/app-server-control/app-server-control.sock`), then connects to it
as a **WebSocket-over-unix** client (raw JSON is dropped — it's WS frames,
JSON-RPC-lite with no `jsonrpc` field, and rejects an `Origin` header). A
plain `codex` auto-attaches to that daemon. The adapter:
- discovers the TUI's live thread from the global `thread/started` broadcast;
- injects framed bus messages with `turn/start{threadId,input,approvalPolicy:"never"}`;
- tracks busy/idle via `thread/status/changed`, one injected turn in flight at
  a time. (It no longer reads turn replies — the agent replies via `jim_send`.)

Caveats: experimental protocol (may shift between codex versions); needs the
daemon (`jimctl codex` starts it); `remote-control` is the WRONG path (it's
cloud/phone control with local transports off).

### pi (`pi` coding CLI) — the `jim-bus.ts` extension IS the integration
The whole pi↔bus integration lives in **one pi extension**,
`integrations/pi/jim-bus.ts`. Install it once:
```bash
cp integrations/pi/jim-bus.ts ~/.pi/agent/extensions/jim-bus.ts
```
It auto-loads in **any** pi process and owns all bus I/O:
- **inbound** — tails the bus and `pi.sendUserMessage()`-injects each
  `agent.inbox.<id>`/`agent.all` message as a real turn, framed with the
  sender and the agent's own id;
- **outbound (the agent's choice)** — registers the tools `jim_send`
  (reply to the asker, message a specific peer, or `to:"all"` to broadcast),
  `jim_roster` (who's online), and `jim_do` (drive the editor). **There is no
  forced auto-reply** — the agent collaborates when and how it decides.
- identity: `$JIM_PI_ID`/`pi-<dir>` + `$JIM_PI_NAME`; `/jim-name`, `/jim-who`.

Run it two ways, same extension:
- **Interactive** — just run `pi`. You watch/steer in the TUI; the agent is
  on the bus.
- **Headless** — `jimctl pi` (below). Background agent, no TUI.

Verified e2e: a bus message injected as a turn and the agent deliberately
called `jim_roster` + `jim_send` to reply point-to-point to the asker.

### pi headless host — `jimctl pi`
A one-command **headless** pi agent on the bus, same id/name flags as
`jimctl codex`:
```bash
jimctl pi                                     # id = pi-<dir>, name = <dir>
jimctl pi --id pi-main --name "pi (frontend)"
```
It is **just the process host**, not a bus bridge: it spawns one
`pi --mode rpc --session-id jim-pi-<id>` (the extension loads and does all bus
I/O), **holds its stdin open** (rpc mode exits the instant stdin hits EOF —
this is the whole reason the host exists), restarts it if it crashes, prints
the agent's replies, and lets you type a line to prompt it locally (`/who`,
`/quit`). The persistent session means context carries across messages and
the agent can do real multi-step work. Refuses to start if the extension
isn't installed (else the agent would be on no bus). Code in
`crates/jimctl/src/cmd_pi.rs`.

> ⚠️ Two pi traps this design avoids. (1) **`pi --mode json --print` never
> exits** — it prints the full turn (`agent_end`) then lingers even with stdin
> closed, so any wait-for-exit read (`Command::output()`) hangs forever *after*
> the answer is already produced, and it throws away the session each turn.
> (2) **`pi --mode rpc` exits on stdin EOF** — a backgrounded `pi --mode rpc &`
> with no stdin dies right after startup. The host keeps stdin open. RPC mode
> is the right long-running tool — see `docs/rpc.md` in the pi package.

Either this headless worker or the live-TUI extension (above) puts a real,
persistent pi session on the bus; pick by whether you want to watch/steer it
in a terminal (extension) or run it as a background delegate (`jimctl pi`).
Follow-up: give the headless worker `jim_send`/`jim_do` so it can initiate,
not just reply (the extension already has them).

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
