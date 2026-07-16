//! `jimctl` — command-line control for the running Jim app.
//!
//! Multi-call dispatcher: `jimctl <command> [args...]`. Each command
//! lives in its own `cmd_*` module and was previously a standalone
//! `tb*` binary (open, msg, …) — the wire formats are still
//! duplicated per-module on purpose.
//!
//! Kept deliberately lib-free of the GUI crate (`jim_app`, which links
//! the libghostty-vt dylib) so it ships without an @rpath dance. The
//! only dependency is the dylib-free `jim_daemon` (used by `inject`).

use std::process::ExitCode;

mod agent_bus;
mod project_resolve;
mod cmd_agent;
mod cmd_channel;
mod cmd_mcp;
mod cmd_codex;
mod cmd_pi;
mod cmd_close;
mod cmd_dock;
mod cmd_git;
mod cmd_inbox;
mod cmd_inject;
mod cmd_issue;
mod cmd_lsp;
mod cmd_memory;
mod cmd_msg;
mod cmd_open;
mod cmd_project;
mod cmd_review;
mod cmd_suggest;
mod cmd_trace;
mod cmd_widget;

/// Args after the subcommand — argv with prog + subcommand stripped.
/// The `cmd_*` modules were written against `std::env::args().skip(1)`
/// when they were standalone bins; this restores that view.
pub fn sub_args() -> impl Iterator<Item = String> {
    std::env::args().skip(2)
}

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("open") => cmd_open::run(),
        Some("widget") => cmd_widget::run(),
        Some("channel") => cmd_channel::run(),
        Some("codex") => cmd_codex::run(),
        Some("pi") => cmd_pi::run(),
        Some("agent") => cmd_agent::run(),
        Some("mcp") => cmd_mcp::run(),
        Some("inbox") => cmd_inbox::run(),
        Some("project") => cmd_project::run(),
        Some("suggest") => cmd_suggest::run(),
        Some("git") => cmd_git::run(),
        Some("review") => cmd_review::run(),
        Some("msg") => cmd_msg::run(),
        Some("close") => cmd_close::run(),
        Some("dock") => cmd_dock::run(),
        Some("issue") => cmd_issue::run(),
        Some("lsp") => cmd_lsp::run(),
        Some("memory") => cmd_memory::run(),
        Some("inject") => cmd_inject::run(),
        Some("trace") => cmd_trace::run(),
        // Host the message-bus daemon. Spawned by self-exec from any
        // client (GUI or CLI) that finds the bus down — see jim_bus.
        Some(s) if s == jim_bus::DAEMON_ARG => {
            jim_bus::daemon::daemonize_if_requested();
            match jim_bus::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("[jim-bus] fatal: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("jimctl: unknown command '{other}'\n");
            usage();
            ExitCode::FAILURE
        }
        None => {
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "usage: jimctl <command> [args...]\n\
         \n\
         commands:\n\
         \topen <file> [--project NAME]   open a file in an editor pane\n\
         \twidget ...                     spawn/control a widget pane\n\
         \tchannel                        MCP channel bridging a Claude session to the agent bus\n\
         \tcodex                          bridge a Codex (codex-cli) session onto the agent bus\n\
         \tpi                             bridge a pi session onto the agent bus\n\
         \tagent ...                      bus convenience layer (roster/send/recv/announce)\n\
         \tmcp                            MCP stdio server: jim_send/jim_roster/jim_do tools for any agent\n\
         \tinbox ...                      push to / read a project's inbox\n\
         \tproject ...                    project operations\n\
         \tsuggest ...                    park a pane in the suggestion drawer\n\
         \tgit ...                        git queries + safe mutations + hunk staging (JSON out)\n\
         \treview ...                     local code-review threads (list/add/reply/resolve)\n\
         \tmsg <topic> <body>             publish on the widget message bus\n\
         \tclose ...                      close a pane\n\
         \tdock --project P [--title T ...]  snap panes into a dock (sidebar+main)\n\
         \tissue ...                      issue-tracker operations\n\
         \tlsp ...                        structural code queries via rust-analyzer (ensure/symbols/source/rpc)\n\
         \tmemory ...                     manage the DeepSeek planner's memory\n\
         \tinject ...                     send keystrokes into a session\n\
         \ttrace [--arm|--disarm] [--ms N]  control the frame-trace recorder at runtime"
    );
}
