/**
 * jim-bus.ts — a pi extension that puts THIS interactive pi session on the
 * jim agent bus (see AGENTS-ON-THE-BUS.md). Unlike `jimctl pi` (a headless
 * worker), this runs *inside* your real pi TUI.
 *
 *   inbound:  agent.inbox.<id> / agent.all  →  pi.sendUserMessage (a real turn,
 *             shown in your session)
 *   outbound: the agent's reply to a bus prompt → back to the asker's inbox
 *             (point-to-point); plus jim_send / jim_do tools the agent can
 *             call deliberately.
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
  // Sender of the bus prompt currently being answered (null = user typed it).
  let currentAsker: string | null = null;

  const announce = () =>
    publish(`agent.hello.${id}`, { id, pid: process.pid, cwd: process.cwd(), label: name }, true, id);
  const tombstone = () => publish(`agent.hello.${id}`, null, true, id);

  // ---- outbound: tools the agent can call deliberately ----
  pi.registerTool({
    name: "jim_send",
    label: "jim send",
    description: "Message another agent on the jim bus. to = an agent id, or 'all' to broadcast.",
    promptGuidelines: ["Use jim_send to message another agent or broadcast on the jim bus."],
    parameters: Type.Object({ to: Type.String(), text: Type.String() }),
    async execute(_cid: string, params: { to: string; text: string }) {
      const to = String(params.to).replace(/^agent:/, "");
      const topic = to === "all" ? "agent.all" : `agent.inbox.${to}`;
      publish(topic, { from: id, text: params.text }, false, id);
      return { content: [{ type: "text", text: `sent to ${topic}` }], details: {} };
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
      // A marker the reply-router parses back out in before_agent_start.
      pi.sendUserMessage(`[jim from=${from}] ${text}`);
    }
  }, 250);

  // ---- outbound auto-reply: send the answer back to whoever asked ----
  pi.on("before_agent_start", async (event: any) => {
    const mt = /^\[jim from=([^\]]+)\]\s/.exec(event.prompt || "");
    currentAsker = mt ? mt[1] : null; // null ⇒ a locally-typed prompt
  });
  pi.on("agent_end", async (event: any) => {
    if (!currentAsker) return; // local prompt: don't echo to the bus
    const asker = currentAsker;
    currentAsker = null;
    let reply = "";
    const msgs = event.messages || [];
    for (let i = msgs.length - 1; i >= 0; i--) {
      if (msgs[i].role !== "assistant") continue;
      const parts = (msgs[i].content || [])
        .filter((c: any) => c.type === "text")
        .map((c: any) => c.text);
      if (parts.length) {
        reply = parts.join("");
        break;
      }
    }
    if (reply) publish(`agent.inbox.${asker}`, { from: id, to: asker, text: reply }, false, id);
  });

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
