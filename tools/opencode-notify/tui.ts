import { closeSync, openSync, writeSync } from "node:fs"
import { Plugin } from "@opencode/plugin/tui"

export default Plugin.define({
  id: "jim.opencode-notify",
  setup(context) {
    const inJim = Boolean(process.env.EDITOR_IDEA_TERMINAL_SESSION_ID)
    const owned = new Set<string>()
    const notified = new Set<string>()
    const current = (sessionID: string) => {
      const route = context.ui.router.current()
      return route.type === "session" && route.sessionID === sessionID
    }
    const notify = (sessionID: string, message: string) => {
      if (!owned.has(sessionID) && !current(sessionID)) return
      const session = context.data.session.get(sessionID)
      if (!session || session.parentID !== undefined || notified.has(sessionID)) return
      notified.add(sessionID)
      owned.delete(sessionID)
      if (inJim) {
        // Emit from the CLI, not the shared server, to reach the owning pane.
        // Jim applies its own project visibility check; never gate on TUI focus.
        try {
          const tty = openSync("/dev/tty", "w")
          try {
            writeSync(tty, "\x07")
          } finally {
            closeSync(tty)
          }
        } catch (error) {
          context.ui.toast.show({ message: `Jim notification failed: ${error}`, variant: "error" })
        }
        return
      }
      void context.attention.notify({
        title: session.title ?? "OpenCode",
        message,
        notification: { when: "blurred" },
        sound: false,
      }).catch((error) => {
        context.ui.toast.show({ message: `Notification failed: ${error}`, variant: "error" })
      })
    }
    const stops = [
      context.data.on("session.execution.started", (event) => {
        notified.delete(event.data.sessionID)
        if (current(event.data.sessionID)) owned.add(event.data.sessionID)
      }),
      context.data.on("session.execution.succeeded", (event) => {
        notify(event.data.sessionID, "Session done")
      }),
      context.data.on("session.execution.failed", (event) => {
        notify(event.data.sessionID, "Session failed")
      }),
      context.data.on("session.execution.interrupted", (event) => {
        if (event.data.reason === "shutdown" || event.data.reason === "superseded") return
        notify(event.data.sessionID, "Session stopped")
      }),
    ]
    return () => stops.reverse().forEach((stop) => stop())
  },
})
