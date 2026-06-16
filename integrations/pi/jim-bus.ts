/**
 * jim-bus.ts — a pi extension that puts THIS pi session on the jim agent bus
 * (see AGENTS-ON-THE-BUS.md). Works the same in an interactive `pi` TUI
 * (watch/steer it) and a headless `pi --mode rpc` (background agent) — this is
 * the one integration for both.
 *
 *   inbound:  agent.inbox.<id> / agent.all  →  pi.sendUserMessage (a real turn,
 *             shown in your session), framed so the agent knows the sender and
 *             that it may respond.
 *   outbound: the agent COLLABORATES DELIBERATELY via the jim_send tool —
 *             reply to whoever asked, message another agent, or broadcast to
 *             all. Plus jim_do to drive the editor. There is NO forced
 *             auto-reply: the agent replies when (and to whom) it chooses.
 *
 * Identity: fixed id ($JIM_PI_ID, else pi-<dir>); name ($JIM_PI_NAME, else
 * <dir>) — change live with /jim-name. /jim-who shows it.
 *
 * Install: copy to ~/.pi/agent/extensions/jim-bus.ts (auto-discovered;
 * /reload to hot-reload).
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const SOCK = path.join(os.homedir(), ".jim", "socket");
const LOG = path.join(os.homedir(), ".jim", "widget-bus.log");

/** Publish one message onto the jim bus (the `widget_message` IPC action). */
function publish(topic: string, payload: unknown, retain: boolean, sender: string): void {
  try {
    const req = JSON.stringify({
      action: "widget_message",
      project: "global",
      topic,
      payload,
      retain,
      sender,
    });
    const c = net.createConnection(SOCK);
    c.on("error", () => {});
    c.on("connect", () => c.end(req));
  } catch {
    /* bus/app not running — ignore */
  }
}

export default function (pi: ExtensionAPI) {
  const id = process.env.JIM_PI_ID || `pi-${path.basename(process.cwd())}`;
  let name = process.env.JIM_PI_NAME || path.basename(process.cwd());

  const announce = () =>
    publish(`agent.hello.${id}`, { id, pid: process.pid, cwd: process.cwd(), label: name }, true, id);
  const tombstone = () => publish(`agent.hello.${id}`, null, true, id);

  // ---- outbound: tools the agent can call deliberately ----
  pi.registerTool({
    name: "jim_send",
    label: "jim send",
    description:
      "Send a message to other agents on the jim bus. `to` is an agent id (to " +
      "reply to whoever messaged you, or to reach a specific peer), or 'all' to " +
      "broadcast to every agent. Use this to reply, ask a peer for help, hand off " +
      "work, or announce something. Find live agents and their ids with jim_roster.",
    promptGuidelines: [
      "When you receive a [jim bus] message and want to respond, reply with the " +
        "jim_send tool (to = the sender's id). You are not required to reply — " +
        "only do so when it's useful.",
      "Use jim_send (to='all') to broadcast, or to=<agent id> to message a specific peer.",
    ],
    parameters: Type.Object({ to: Type.String(), text: Type.String() }),
    async execute(_cid: string, params: { to: string; text: string }) {
      const to = String(params.to).replace(/^agent:/, "");
      const topic = to === "all" ? "agent.all" : `agent.inbox.${to}`;
      publish(topic, { from: id, text: params.text }, false, id);
      return { content: [{ type: "text", text: `sent to ${topic}` }], details: {} };
    },
  });

  pi.registerTool({
    name: "jim_roster",
    label: "jim roster",
    description:
      "List the other agents currently live on the jim bus (their ids, names, and " +
      "working dirs) so you know who you can jim_send to.",
    parameters: Type.Object({}),
    async execute() {
      const lines: string[] = [];
      try {
        const text = fs.readFileSync(LOG, "utf8");
        const latest = new Map<string, any>();
        for (const line of text.split("\n")) {
          if (!line.trim()) continue;
          let m: any;
          try {
            m = JSON.parse(line);
          } catch {
            continue;
          }
          const t: string = m.topic || "";
          if (!t.startsWith("agent.hello.")) continue;
          const sid = t.slice("agent.hello.".length);
          if (m.payload == null) latest.delete(sid);
          else latest.set(sid, m.payload);
        }
        for (const [sid, info] of latest) {
          if (sid === id) continue; // skip self
          const label = info && info.label ? ` "${info.label}"` : "";
          const cwd = info && info.cwd ? `  ${info.cwd}` : "";
          lines.push(`${sid}${label}${cwd}`);
        }
      } catch {
        /* bus log absent */
      }
      const out = lines.length ? lines.join("\n") : "no other agents on the bus";
      return { content: [{ type: "text", text: out }], details: {} };
    },
  });

  pi.registerTool({
    name: "jim_do",
    label: "jim do",
    description:
      "Drive the jim editor: dispatch an editor action (open_file, spawn_widget, add_issue, …). `params` are that action's fields.",
    parameters: Type.Object({ action: Type.String(), params: Type.Optional(Type.Any()) }),
    async execute(_cid: string, p: { action: string; params?: Record<string, unknown> }) {
      const payload = { ...(p.params && typeof p.params === "object" ? p.params : {}), action: p.action };
      publish("jim.action", payload, false, id);
      return { content: [{ type: "text", text: `dispatched ${p.action}` }], details: {} };
    },
  });

  // ---- commands: identity ----
  pi.registerCommand("jim-name", {
    description: "Set this session's jim bus display name",
    handler: async (args: string, ctx: any) => {
      const n = (args || "").trim();
      if (n) {
        name = n;
        announce();
        ctx.ui.notify(`jim name → ${name}`, "info");
      }
    },
  });
  pi.registerCommand("jim-who", {
    description: "Show this session's jim bus identity",
    handler: async (_args: string, ctx: any) => {
      ctx.ui.notify(`jim id=${id} name="${name}"`, "info");
    },
  });

  // ---- inbound: tail the bus log, inject matching messages as user turns ----
  let pos = 0;
  try {
    pos = fs.statSync(LOG).size;
  } catch {
    /* file appears once the app runs */
  }
  const poll = setInterval(() => {
    let size: number;
    try {
      size = fs.statSync(LOG).size;
    } catch {
      return;
    }
    if (size < pos) pos = 0; // truncated on app restart
    if (size <= pos) return;
    let chunk: Buffer;
    try {
      const fd = fs.openSync(LOG, "r");
      chunk = Buffer.alloc(size - pos);
      fs.readSync(fd, chunk, 0, chunk.length, pos);
      fs.closeSync(fd);
    } catch {
      return;
    }
    pos = size;
    for (const line of chunk.toString("utf8").split("\n")) {
      if (!line.trim()) continue;
      let m: any;
      try {
        m = JSON.parse(line);
      } catch {
        continue;
      }
      const topic = m.topic || "";
      if (topic !== `agent.inbox.${id}` && topic !== "agent.all") continue;
      const from = (m.payload && m.payload.from) || m.sender || "";
      if (from === id || m.sender === id) continue; // skip our own
      const text =
        m.payload && typeof m.payload.text === "string" ? m.payload.text : JSON.stringify(m.payload);
      const scope = topic === "agent.all" ? "broadcast" : "direct";
      // Inject as a real turn, framed so the agent knows it's a bus message,
      // who it's from, and how to respond. Replying is the agent's choice via
      // the jim_send tool — there is NO automatic echo of the agent's output
      // back to the bus.
      pi.sendUserMessage(
        `[jim bus · ${scope} message from "${from}"]\n${text}\n\n` +
          `(You are agent "${id}" on the jim bus. To reply, use the jim_send ` +
          `tool with to="${from}". To reach another agent use their id, or ` +
          `to="all" to broadcast. jim_roster lists who's online. Reply once if ` +
          `it's useful, then stop — don't call tools repeatedly.)`,
      );
    }
  }, 250);

  pi.on("session_start", async (_e: any, ctx: any) => {
    announce();
    ctx.ui.notify(`jim bus: ${id}`, "info");
  });

  const bye = () => {
    clearInterval(poll);
    tombstone();
  };
  process.on("exit", bye);
  process.on("SIGINT", () => {
    bye();
    process.exit(0);
  });
}
