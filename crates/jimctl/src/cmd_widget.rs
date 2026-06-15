//! `jimctl widget` — spawn a new widget pane in the running `terminal-bevy`
//! app. Mirrors `open`'s socket dance.
//!
//! Usage:
//!     jimctl widget [--title T] [--cwd D] [--project P] -- <cmd> [args...]
//!     jimctl widget [--title T] [--cwd D] [--project P] <cmd-with-spaces>
//!
//! Two argv shapes are accepted:
//!   - Everything after `--` is taken as `argv` for the child (no shell).
//!     Example: `jimctl widget --title issues -- gh-issues.sh`.
//!   - No `--` → the remaining single positional arg is the command line
//!     and is run through `sh -c`. Example:
//!     `jimctl widget --title issues "gh issue list | jq -c '...'"`.
//!
//! `--cwd` defaults to the caller's current directory so relative paths
//! and scripts that read `$PWD` keep working.
//!
//! Wire format is duplicated here on purpose (same rationale as
//! `open`): a same-package bin links the lib's dylib transitively
//! and we don't want to ship the @rpath dance with this CLI.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum IpcRequest {
    SpawnWidget {
        command: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        /// Optional widget kind override. Default is the subprocess
        /// widget kind. Pass `"script_widget"` to spawn an in-process
        /// funct-scripted widget; `command` is then the script filename
        /// under `~/.jim/widgets/`.
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Per-instance params for a `script_widget`, forwarded to the
        /// funct global `params`. This is how you configure a generic
        /// primitive from the shell: `http.ft` with `{url,out}`, a chart
        /// with `{in,rows_path,x,y}`.
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        position: Option<[f32; 2]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<[f32; 2]>,
    },
}

fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".jim").join("socket"))
}

pub fn run() -> ExitCode {
    // `jimctl widget agent` is an introspection command: print a
    // self-contained description of the widget system for an agent
    // driving it from the shell, instead of trying to spawn a pane.
    if crate::sub_args().next().as_deref() == Some("agent") {
        print!("{}", AGENT_GUIDE);
        return ExitCode::SUCCESS;
    }

    let args = match Args::parse() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            print_usage();
            return ExitCode::from(2);
        }
    };

    let Some(sock) = socket_path() else {
        eprintln!("jimctl widget: $HOME not set; can't locate socket");
        return ExitCode::from(1);
    };

    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "jimctl widget: connect {}: {} (is the terminal-bevy app running?)",
                sock.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let req = IpcRequest::SpawnWidget {
        command: args.command,
        args: args.args,
        title: args.title,
        cwd: args.cwd,
        project: args.project,
        kind: args.kind,
        params: args.params,
        position: args.position,
        size: args.size,
    };
    let body = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("jimctl widget: serialize: {}", e);
            return ExitCode::from(1);
        }
    };
    if let Err(e) = stream.write_all(&body) {
        eprintln!("jimctl widget: write: {}", e);
        return ExitCode::from(1);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    ExitCode::SUCCESS
}

struct Args {
    command: String,
    args: Vec<String>,
    title: Option<String>,
    cwd: Option<PathBuf>,
    project: Option<String>,
    kind: Option<String>,
    params: Option<serde_json::Value>,
    position: Option<[f32; 2]>,
    size: Option<[f32; 2]>,
}

/// Parse "x,y" into a 2-float array.
fn parse_pair(s: &str) -> Option<[f32; 2]> {
    let (a, b) = s.split_once(',')?;
    Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut title: Option<String> = None;
        let mut cwd: Option<PathBuf> = None;
        let mut project: Option<String> = None;
        let mut kind: Option<String> = None;
        let mut params: Option<serde_json::Value> = None;
        let mut position: Option<[f32; 2]> = None;
        let mut size: Option<[f32; 2]> = None;
        let mut positional: Vec<String> = Vec::new();
        let mut argv_mode = false;
        let mut argv_after_dash: Vec<String> = Vec::new();

        let mut it = crate::sub_args();
        while let Some(arg) = it.next() {
            if argv_mode {
                argv_after_dash.push(arg);
                continue;
            }
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--title" | "-t" => {
                    title = Some(
                        it.next()
                            .ok_or_else(|| format!("{} requires a value", arg))?,
                    );
                }
                "--cwd" => {
                    cwd = Some(PathBuf::from(
                        it.next()
                            .ok_or_else(|| format!("{} requires a value", arg))?,
                    ));
                }
                "--project" | "-p" => {
                    project = Some(
                        it.next()
                            .ok_or_else(|| format!("{} requires a value", arg))?,
                    );
                }
                "--kind" | "-k" => {
                    kind = Some(
                        it.next()
                            .ok_or_else(|| format!("{} requires a value", arg))?,
                    );
                }
                "--params" => {
                    let raw = it
                        .next()
                        .ok_or_else(|| format!("{} requires a JSON value", arg))?;
                    params = Some(
                        serde_json::from_str(&raw)
                            .map_err(|e| format!("--params: invalid JSON: {}", e))?,
                    );
                }
                "--pos" | "--position" => {
                    let raw = it
                        .next()
                        .ok_or_else(|| format!("{} requires x,y", arg))?;
                    position = Some(
                        parse_pair(&raw).ok_or_else(|| format!("{} expects x,y", arg))?,
                    );
                }
                "--size" => {
                    let raw = it
                        .next()
                        .ok_or_else(|| format!("{} requires w,h", arg))?;
                    size =
                        Some(parse_pair(&raw).ok_or_else(|| format!("{} expects w,h", arg))?);
                }
                "--" => {
                    argv_mode = true;
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown flag: {}", other));
                }
                other => {
                    positional.push(other.into());
                }
            }
        }

        // Default cwd to caller's PWD so scripts behave as if launched
        // from the shell that ran us.
        if cwd.is_none() {
            cwd = std::env::current_dir().ok();
        }

        let (command, child_args) = if !argv_after_dash.is_empty() {
            let mut it = argv_after_dash.into_iter();
            let head = it.next().expect("non-empty after `--`");
            let rest: Vec<String> = it.collect();
            (head, rest)
        } else if positional.len() == 1 {
            // Single positional → shell command line.
            (positional.into_iter().next().unwrap(), Vec::new())
        } else if positional.is_empty() {
            return Err("missing command — pass `-- <cmd>` or a single quoted shell line".into());
        } else {
            return Err(format!(
                "got {} positional args without `--`; use `-- {} ...` to pass them as argv",
                positional.len(),
                positional[0],
            ));
        };

        Ok(Self {
            command,
            args: child_args,
            title,
            cwd,
            project,
            kind,
            params,
            position,
            size,
        })
    }
}

fn print_usage() {
    eprintln!(
        "jimctl widget [--title T] [--cwd D] [--project P] [--kind K] -- <cmd> [args...]\n\
         jimctl widget [--title T] [--cwd D] [--project P] [--kind K] <shell-line>\n\
         \n\
         Spawn a new widget pane in the running terminal-bevy app. The\n\
         child speaks the widget NDJSON protocol over stdout/stdin.\n\
         \n\
         With `--`, the remaining args become argv and the child runs\n\
         directly. Without `--`, a single quoted positional is passed to\n\
         `sh -c`.\n\
         \n\
         `--kind script_widget` swaps in the in-process funct-scripted\n\
         widget runtime. `<cmd>` is then interpreted as a script filename\n\
         under `~/.jim/widgets/` and no subprocess is spawned.\n\
         \n\
         --params JSON   per-instance config → funct global `params`\n\
         --pos x,y       window-space top-left   --size w,h\n\
         \n\
         Build a dataflow pipe from the shell (source → chart on a topic):\n\
           jimctl widget -k script_widget -p Recursion --pos 40,60 \\\n\
             --params '{{\"url\":\"https://api…\",\"out\":\"feed\"}}' http.ft\n\
           jimctl widget -k script_widget -p Recursion --pos 40,230 \\\n\
             --params '{{\"in\":\"feed\",\"rows_path\":\"items\",\"x\":\"name\",\"y\":\"count\"}}' df_view_bar.ft\n\
         \n\
         For the authoring guide, run `widget agent`."
    );
}

/// Self-contained description of the widget system, printed by
/// `jimctl widget agent`. Aimed at an agent (human or LLM) driving
/// widgets from the shell: what a widget is, how to spawn one, the
/// dataflow pattern, and the message bus. The exhaustive handler/event
/// reference lives in `crates/jim-widget/AUTHORING.md`.
const AGENT_GUIDE: &str = "\
jim widgets — agent guide
=========================

A *widget* is a floating pane that renders a retained UI tree and reacts
to events. Two hosting paths share one Element vocabulary:

  in-process funct  — a `.ft` script in ~/.jim/widgets/, runs on a worker
                      thread, hot-reloads on save. The default; use for
                      small, live-editable UI.
  subprocess        — any program speaking NDJSON (HostEvent in, frame
                      out) over stdio. Use for heavier logic / isolation.

Spawning from the shell
-----------------------
  jimctl widget [--title T] [--cwd D] [--project P] [--kind K] \\
                [--params JSON] [--pos x,y] [--size w,h] -- <cmd> [args]
  jimctl widget [flags] <shell-line>          # single arg → run via sh -c

  --kind script_widget   host an in-process funct script; <cmd> is then a
                         script filename under ~/.jim/widgets/ (e.g.
                         http.ft) and NO subprocess is spawned.
  --params JSON          per-instance config → the funct global `params`.
                         This is how a generic primitive is configured.
  --pos / --size         window-space placement (floats, `x,y`).
  --project / -p         which editor project the pane belongs to.

Default kind (no --kind) is the subprocess widget. Default --cwd is the
caller's PWD.

The dataflow pattern
--------------------
Widgets pipe data over named topics on the widget↔widget bus: a *source*
fetches and publishes onto a topic, a *view* subscribes and charts it.
Both are generic `.ft` primitives configured purely via --params — never
write a per-endpoint script.

  # source: poll a URL, publish JSON onto topic `feed`
  jimctl widget -k script_widget -p Recursion --pos 40,60 \\
    --params '{\"url\":\"https://api…\",\"out\":\"feed\"}' http.ft

  # view: render rows from `feed` as a bar chart
  jimctl widget -k script_widget -p Recursion --pos 40,230 \\
    --params '{\"in\":\"feed\",\"rows_path\":\"items\",\"x\":\"name\",\"y\":\"count\"}' \\
    df_view_bar.ft

View primitives: df_view_bar / line / multiline / vbars / heatmap / stat
/ table — all keyed by the same {in, rows_path, x, y} param shape.

The buses (keep them straight)
------------------------------
  UI events       — clicks/toggles/input from THIS widget's own elements;
                    reach funct handlers on_click/on_toggle/on_input_*.
  Claude Code bus — pre_tool_use/stop/… mirrored from the hook bus; every
                    widget sees every event → handler `on_bus(kind,pl)`.
  widget↔widget   — control messages widgets send each other, scoped to
                    one project → `emit`/`on_message`. Drive it from the
                    shell with `jimctl msg`:
                        jimctl msg emit -p PROJ --topic t --json '{…}'
                        jimctl msg tail -p PROJ        # follow live

Authoring scripts
-----------------
Drop a `.ft` in ~/.jim/widgets/ (hot-reloaded). Define `fn render(w,h)`
returning an Element map, plus only the handlers you need. Widgets are
event-driven: after mutating `state`, call request_render(); opt into
animation with set_animating(true) → on_frame(dt). Any `.ft` with a
`render` fn auto-registers as a `widget.<stem>` palette action.

Full reference (handler↔event tables, Element catalog, subprocess
protocol, bus conventions): crates/jim-widget/AUTHORING.md and the
dataflow spec in DATAFLOW.md.
";
