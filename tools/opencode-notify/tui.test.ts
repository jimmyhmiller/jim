import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test"

const open = mock(() => 42)
const write = mock(() => 1)
const close = mock(() => {})
mock.module("node:fs", () => ({ openSync: open, writeSync: write, closeSync: close }))
mock.module("@opencode/plugin/tui", () => ({ Plugin: { define: (plugin: unknown) => plugin } }))
const { default: plugin } = await import("./tui")

type Event = { data: { sessionID: string; reason?: string } }
const originalTerminal = process.env.EDITOR_IDEA_TERMINAL_SESSION_ID
let listeners: Map<string, (event: Event) => void>
let route: { type: string; sessionID?: string }
let sessions: Map<string, { title: string; parentID?: string }>
let attention: ReturnType<typeof mock>
let toast: ReturnType<typeof mock>
let cleanup: () => void

function setup(inJim = true) {
  if (inJim) process.env.EDITOR_IDEA_TERMINAL_SESSION_ID = "123"
  else delete process.env.EDITOR_IDEA_TERMINAL_SESSION_ID
  cleanup = plugin.setup({
    data: {
      on(type: string, listener: (event: Event) => void) {
        listeners.set(type, listener)
        return () => { listeners.delete(type) }
      },
      session: { get: (id: string) => sessions.get(id) },
    },
    ui: { router: { current: () => route }, toast: { show: toast } },
    attention: { notify: attention },
  } as never) as () => void
}

function emit(outcome: string, sessionID = "ses_root", reason?: string) {
  listeners.get(`session.execution.${outcome}`)?.({ data: { sessionID, reason } })
}

beforeEach(() => {
  open.mockReset()
  open.mockImplementation(() => 42)
  write.mockClear()
  close.mockClear()
  listeners = new Map()
  route = { type: "session", sessionID: "ses_root" }
  sessions = new Map([
    ["ses_root", { title: "Root" }],
    ["ses_child", { title: "Child", parentID: "ses_root" }],
    ["ses_other", { title: "Other" }],
  ])
  attention = mock(() => Promise.resolve({ ok: true }))
  toast = mock(() => {})
})

afterEach(() => {
  cleanup?.()
  if (originalTerminal === undefined) delete process.env.EDITOR_IDEA_TERMINAL_SESSION_ID
  else process.env.EDITOR_IDEA_TERMINAL_SESSION_ID = originalTerminal
})

describe("Jim stop notifications", () => {
  for (const outcome of ["succeeded", "failed", "interrupted"]) {
    test(`${outcome} emits one BEL through the controlling terminal`, () => {
      setup()
      emit(outcome)
      expect(open).toHaveBeenCalledWith("/dev/tty", "w")
      expect(write).toHaveBeenCalledWith(42, "\x07")
      expect(close).toHaveBeenCalledWith(42)
      expect(attention).not.toHaveBeenCalled()
    })
  }

  test("duplicate stops notify once, and the next turn notifies again", () => {
    setup()
    emit("succeeded")
    emit("succeeded")
    emit("failed")
    expect(write).toHaveBeenCalledTimes(1)
    emit("started")
    emit("succeeded")
    expect(write).toHaveBeenCalledTimes(2)
  })

  test("ignores child sessions and sessions owned by another CLI", () => {
    setup()
    emit("started", "ses_other")
    emit("succeeded", "ses_other")
    route.sessionID = "ses_child"
    emit("started", "ses_child")
    emit("succeeded", "ses_child")
    expect(write).not.toHaveBeenCalled()
  })

  test("notifies after navigating away from a running session", () => {
    setup()
    emit("started")
    route = { type: "home" }
    emit("succeeded")
    expect(write).toHaveBeenCalledTimes(1)
  })

  test("ignores shutdown and superseded interruptions", () => {
    setup()
    emit("interrupted", "ses_root", "shutdown")
    emit("interrupted", "ses_root", "superseded")
    expect(write).not.toHaveBeenCalled()
    emit("interrupted", "ses_root", "user")
    expect(write).toHaveBeenCalledTimes(1)
  })

  test("reports terminal write errors without crashing the CLI", () => {
    setup()
    open.mockImplementation(() => { throw new Error("no tty") })
    emit("succeeded")
    expect(toast).toHaveBeenCalledTimes(1)
    expect(write).not.toHaveBeenCalled()
  })

  test("retains desktop notifications outside Jim", () => {
    setup(false)
    emit("succeeded")
    expect(write).not.toHaveBeenCalled()
    expect(attention).toHaveBeenCalledWith({
      title: "Root",
      message: "Session done",
      notification: { when: "blurred" },
      sound: false,
    })
  })

  test("cleanup unsubscribes every event", () => {
    setup()
    expect(listeners.size).toBe(4)
    cleanup()
    expect(listeners.size).toBe(0)
  })
})
