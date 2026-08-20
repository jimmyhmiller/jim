//! `jimctl pad` — write cells into a notebook that a `pad.ft` pane renders live.
//!
//! A notebook is a JSONL file in `~/.jim/pads/<name>.jsonl`: one record per
//! line, reduce them in order and you have the document. This command appends
//! records; the pane watches for a bus doorbell, re-reads the file, and draws.
//! That split is deliberate — the bus carries control signals, not bulk data,
//! so the payload here is "notebook X changed", never the cell itself. It also
//! means the notebook survives a GUI restart and a late-opening pane sees the
//! whole history, neither of which a retained bus message can give us.
//!
//!   jimctl pad new incident-42
//!   jimctl pad md "# Checkout latency spike"
//!   jimctl pad chart '{"type":"line","series":[{"points":[["2026-08-01",120]]}]}'
//!   jimctl pad stats --id migrated '[{"label":"Files","value":12}]'
//!
//! Every add prints the cell's id. Reusing an id with `--id` REPLACES that
//! cell in place, which is how a long-running agent keeps one progress figure
//! current instead of appending a hundred of them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Paths + state
// ---------------------------------------------------------------------------

/// `$HOME/.jim/pads`, matching how every other jimctl command and the bus
/// locate their state — no env override, so the CLI and the pane can never
/// disagree about where a notebook lives.
fn pads_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".jim").join("pads")
}

fn state_path() -> PathBuf {
    pads_dir().join("state.json")
}

fn notebook_path(name: &str) -> PathBuf {
    pads_dir().join(format!("{name}.jsonl"))
}

fn assets_dir(name: &str) -> PathBuf {
    pads_dir().join("assets").join(name)
}

/// A notebook name has to be usable as a file name and as a bus topic
/// segment, so keep it to the characters that are unambiguous in both.
fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a notebook name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err(format!("notebook name is too long ({} chars, max 64)", name.len()));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'))
    {
        return Err(format!(
            "notebook name {name:?} contains {bad:?}; use letters, digits, '-', '_' or '.'"
        ));
    }
    if name.starts_with('.') {
        return Err(format!("notebook name {name:?} cannot start with a dot"));
    }
    Ok(())
}

fn read_state() -> Value {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_state(v: &Value) -> Result<(), String> {
    let dir = pads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = state_path();
    std::fs::write(&path, format!("{v:#}\n")).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn current_notebook() -> String {
    read_state()
        .get("current")
        .and_then(|v| v.as_str())
        .unwrap_or("scratch")
        .to_string()
}

fn set_current(name: &str) -> Result<(), String> {
    let mut st = read_state();
    st["current"] = json!(name);
    write_state(&st)
}

// ---------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------

fn read_log(name: &str) -> String {
    std::fs::read_to_string(notebook_path(name)).unwrap_or_default()
}

/// Ids already used in this notebook, so a generated one can't collide with a
/// cell the reader is looking at.
fn existing_ids(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A short id, derived from the clock and salted until it's unused. Short
/// because it's meant to be typed back in (`pad rm c3f9`).
fn fresh_id(log: &str) -> String {
    let taken = existing_ids(log);
    let mut seed = now_ms().wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    for _ in 0..1000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let id = format!("{:04x}", (seed >> 33) as u16);
        if !taken.iter().any(|t| *t == id) {
            return id;
        }
    }
    // 65k ids exhausted in one notebook: fall back to something unmistakably
    // unique rather than silently reusing an id and overwriting a cell.
    format!("c{}", now_ms())
}

fn append_record(name: &str, rec: &Value) -> Result<(), String> {
    let dir = pads_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = notebook_path(name);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    writeln!(f, "{rec}").map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Ring the doorbell: the pane re-reads the file when it hears this.
///
/// Published globally rather than per-project — a notebook is identified by
/// name, so a pad pane should see its own notebook's changes no matter which
/// project it happens to be sitting in. A failure here is a warning, not an
/// error: the record is already durable on disk, and the pane will pick it up
/// on its next read.
fn notify(name: &str, op: &str, id: &str) {
    let payload = json!({ "notebook": name, "op": op, "id": id });
    let msg = jim_bus::proto::BusMessage {
        project: None,
        topic: format!("pad.{name}"),
        payload_json: payload.to_string(),
        sender: "jimctl pad".to_string(),
        retain: false,
    };
    if let Err(e) = jim_bus::client::publish_oneshot(&msg) {
        eprintln!("jimctl pad: note — the cell was written but the pane was not notified ({e})");
    }
}

/// Append a cell and report its id. `id` empty = generate one (append);
/// otherwise the cell with that id is replaced in place.
fn put_cell(name: &str, id: &str, cell: Value) -> Result<String, String> {
    let log = read_log(name);
    let id = if id.is_empty() {
        fresh_id(&log)
    } else {
        id.to_string()
    };
    let rec = json!({ "op": "update", "id": id, "ts": now_ms(), "cell": cell });
    append_record(name, &rec)?;
    notify(name, "update", &id);
    Ok(id)
}

// ---------------------------------------------------------------------------
// Arg handling
// ---------------------------------------------------------------------------

struct Args {
    positional: Vec<String>,
    named: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Args {
    fn parse(args: &[String]) -> Args {
        let mut positional = Vec::new();
        let mut named = Vec::new();
        let mut switches = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(rest) = a.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    named.push((k.to_string(), v.to_string()));
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    named.push((rest.to_string(), args[i + 1].clone()));
                    i += 1;
                } else {
                    switches.push(rest.to_string());
                }
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Args {
            positional,
            named,
            switches,
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.named
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, key: &str) -> bool {
        self.switches.iter().any(|s| s == key)
    }

    fn notebook(&self) -> String {
        self.get("notebook")
            .or_else(|| self.get("n"))
            .map(str::to_string)
            .unwrap_or_else(current_notebook)
    }

    fn id(&self) -> String {
        self.get("id").unwrap_or("").to_string()
    }

    /// First positional argument, or all of stdin when it's absent. This is
    /// what makes heredocs work: `pad md <<'EOF' … EOF`.
    fn body_or_stdin(&self) -> Result<String, String> {
        if let Some(first) = self.positional.first() {
            return Ok(first.clone());
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        if buf.trim().is_empty() {
            return Err("nothing to write: pass the content as an argument or on stdin".into());
        }
        Ok(buf)
    }

    /// Like `body_or_stdin`, but a positional naming an existing file reads
    /// that file — `pad code src/main.rs` and `pad chart spec.json`.
    fn body_file_or_stdin(&self) -> Result<(String, Option<PathBuf>), String> {
        if let Some(first) = self.positional.first() {
            let p = Path::new(first);
            if p.is_file() {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("reading {}: {e}", p.display()))?;
                return Ok((text, Some(p.to_path_buf())));
            }
            return Ok((first.clone(), None));
        }
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        if buf.trim().is_empty() {
            return Err("nothing to write: pass the content as an argument, a file path, or on stdin".into());
        }
        Ok((buf, None))
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() -> ExitCode {
    let argv: Vec<String> = crate::sub_args().collect();
    let Some(sub) = argv.first().cloned() else {
        print_usage();
        return ExitCode::from(2);
    };
    let args = Args::parse(&argv[1..]);

    let result = match sub.as_str() {
        "new" => cmd_new(&args),
        "use" => cmd_use(&args),
        "list" => cmd_list(),
        "current" => {
            println!("{}", current_notebook());
            Ok(())
        }
        "path" => {
            println!("{}", notebook_path(&args.notebook()).display());
            Ok(())
        }
        "md" | "markdown" => cmd_md(&args),
        "callout" => cmd_callout(&args),
        "code" => cmd_code(&args),
        "table" => cmd_table(&args),
        "chart" => cmd_chart(&args),
        "stats" => cmd_stats(&args),
        "json" => cmd_json(&args),
        "image" => cmd_image(&args),
        "graph" => cmd_graph(&args),
        "rm" => cmd_rm(&args),
        "clear" => cmd_clear(&args),
        "-h" | "--help" | "help" => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown subcommand `{other}` (try `jimctl pad help`)")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jimctl pad: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: jimctl pad <command> [args]\n\
         \n\
         notebooks:\n\
         \tnew <name>              create a notebook and make it current\n\
         \tuse <name>              switch the current notebook\n\
         \tlist                    list notebooks\n\
         \tcurrent                 print the current notebook's name\n\
         \tpath                    print the current notebook's file path\n\
         \n\
         cells (each prints the new cell's id):\n\
         \tmd [TEXT]               markdown (stdin when TEXT is omitted)\n\
         \tcallout <level> [TEXT]  info | success | warn | error\n\
         \tcode [FILE|TEXT]        syntax-highlighted code\n\
         \ttable <JSON|CSV|FILE>   array of objects, {{columns, rows}}, or CSV\n\
         \tchart <JSON|FILE>       line | area | scatter | bar | donut | histogram\n\
         \tstats <JSON>            [{{\"label\",\"value\",\"delta\"?,\"spark\"?}}]\n\
         \tjson <JSON|FILE>        collapsible JSON tree\n\
         \timage <PATH>            copied into the notebook's assets\n\
         \tgraph <DOT|FILE>        Graphviz DOT diagram\n\
         \trm <id>                 remove one cell\n\
         \tclear                   remove every cell\n\
         \n\
         flags:\n\
         \t--notebook N            write to N instead of the current notebook\n\
         \t--id ID                 replace the cell with this id, in place\n\
         \t--title T               title, where the cell kind has one\n\
         \t--lang L                language for `code`\n\
         \t--caption C / --alt A   for `image`"
    );
}

// ---------------------------------------------------------------------------
// Notebook management
// ---------------------------------------------------------------------------

fn cmd_new(args: &Args) -> Result<(), String> {
    let name = args
        .positional
        .first()
        .ok_or("usage: jimctl pad new <name>")?;
    valid_name(name)?;
    let path = notebook_path(name);
    if path.exists() && !args.has("force") {
        return Err(format!(
            "{} already exists — `jimctl pad use {name}` to switch to it, or pass --force to start it over",
            path.display()
        ));
    }
    std::fs::create_dir_all(pads_dir()).map_err(|e| format!("creating {}: {e}", pads_dir().display()))?;
    std::fs::write(&path, "").map_err(|e| format!("writing {}: {e}", path.display()))?;
    set_current(name)?;
    notify(name, "clear", "");
    println!("{}", path.display());
    Ok(())
}

fn cmd_use(args: &Args) -> Result<(), String> {
    let name = args
        .positional
        .first()
        .ok_or("usage: jimctl pad use <name>")?;
    valid_name(name)?;
    if !notebook_path(name).exists() {
        return Err(format!(
            "no notebook named {name:?} — `jimctl pad new {name}` creates it"
        ));
    }
    set_current(name)?;
    println!("{name}");
    Ok(())
}

fn cmd_list() -> Result<(), String> {
    let dir = pads_dir();
    let current = current_notebook();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("no notebooks yet — `jimctl pad new <name>` creates one");
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    if names.is_empty() {
        println!("no notebooks yet — `jimctl pad new <name>` creates one");
        return Ok(());
    }
    for n in names {
        let marker = if n == current { "*" } else { " " };
        let cells = read_log(&n).lines().filter(|l| !l.trim().is_empty()).count();
        println!("{marker} {n}  ({cells} records)");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

fn emit(args: &Args, cell: Value) -> Result<(), String> {
    let name = args.notebook();
    valid_name(&name)?;
    let id = put_cell(&name, &args.id(), cell)?;
    println!("{id}");
    Ok(())
}

fn cmd_md(args: &Args) -> Result<(), String> {
    let source = args.body_or_stdin()?;
    emit(args, json!({ "type": "markdown", "source": source }))
}

fn cmd_callout(args: &Args) -> Result<(), String> {
    let level = args
        .positional
        .first()
        .map(|s| s.to_ascii_lowercase())
        .ok_or("usage: jimctl pad callout <info|success|warn|error> [TEXT]")?;
    if !matches!(level.as_str(), "info" | "success" | "warn" | "error") {
        return Err(format!(
            "callout level {level:?} is not one of info, success, warn, error"
        ));
    }
    let source = match args.positional.get(1) {
        Some(s) => s.clone(),
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("reading stdin: {e}"))?;
            if buf.trim().is_empty() {
                return Err("nothing to write: pass the callout body as an argument or on stdin".into());
            }
            buf
        }
    };
    emit(
        args,
        json!({ "type": "callout", "level": level, "source": source }),
    )
}

/// Language for a code cell: `--lang` wins, else the file extension, else
/// plain text (which highlights as nothing, rather than guessing wrong).
fn language_for(args: &Args, path: Option<&Path>) -> Option<String> {
    if let Some(l) = args.get("lang") {
        return Some(l.to_string());
    }
    path.and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(str::to_string)
}

fn cmd_code(args: &Args) -> Result<(), String> {
    let (source, path) = args.body_file_or_stdin()?;
    let mut cell = json!({ "type": "code", "source": source });
    if let Some(lang) = language_for(args, path.as_deref()) {
        cell["language"] = json!(lang);
    }
    let title = args.get("title").map(str::to_string).or_else(|| {
        path.as_deref()
            .and_then(|p| p.file_name())
            .and_then(|f| f.to_str())
            .map(str::to_string)
    });
    if let Some(t) = title {
        cell["title"] = json!(t);
    }
    emit(args, cell)
}

fn cmd_table(args: &Args) -> Result<(), String> {
    let (raw, path) = args.body_file_or_stdin()?;
    let looks_json = raw.trim_start().starts_with('[') || raw.trim_start().starts_with('{');
    let (columns, rows) = if looks_json {
        table_from_json(&raw)?
    } else {
        table_from_csv(&raw)?
    };
    let mut cell = json!({ "type": "table", "columns": columns, "rows": rows });
    let title = args.get("title").map(str::to_string).or_else(|| {
        path.as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|f| f.to_str())
            .map(str::to_string)
    });
    if let Some(t) = title {
        cell["title"] = json!(t);
    }
    emit(args, cell)
}

/// One table row, keeping its keys in the order the document wrote them.
///
/// `serde_json::Value` stores objects in a `BTreeMap`, so parsing a row into a
/// `Value` alphabetizes its keys — a table written `{"region":…, "p95":…}`
/// would come out with p95 first. Visiting the map directly keeps the author's
/// column order, which is the whole point of handing us objects.
struct OrderedRow(Vec<(String, Value)>);

impl<'de> serde::Deserialize<'de> for OrderedRow {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct RowVisitor;
        impl<'de> serde::de::Visitor<'de> for RowVisitor {
            type Value = OrderedRow;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a table row object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<OrderedRow, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(OrderedRow(out))
            }
        }
        d.deserialize_map(RowVisitor)
    }
}

/// `[{...}, {...}]` (keys become columns, union in first-seen order) or
/// `{"columns": [...], "rows": [[...]]}`.
fn table_from_json(raw: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    let v: Value =
        serde_json::from_str(raw).map_err(|e| format!("table data is not valid JSON: {e}"))?;
    if let Some(obj) = v.as_object() {
        let columns: Vec<String> = obj
            .get("columns")
            .and_then(|c| c.as_array())
            .ok_or("table object needs a `columns` array")?
            .iter()
            .map(|c| c.as_str().unwrap_or_default().to_string())
            .collect();
        let rows: Vec<Vec<Value>> = obj
            .get("rows")
            .and_then(|r| r.as_array())
            .ok_or("table object needs a `rows` array")?
            .iter()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect();
        return Ok((columns, rows));
    }
    if !v.is_array() {
        return Err("table data must be an array of objects or {columns, rows}".into());
    }
    // Re-read the array as ordered key/value PAIRS rather than as maps.
    // `serde_json::Map` is a BTreeMap here, so going through `Value` would
    // alphabetize the columns and silently reorder the author's table.
    let arr: Vec<OrderedRow> = serde_json::from_str(raw)
        .map_err(|_| "every element of a table array must be an object".to_string())?;
    let mut columns: Vec<String> = Vec::new();
    for row in &arr {
        for (k, _) in &row.0 {
            if !columns.iter().any(|c| c == k) {
                columns.push(k.clone());
            }
        }
    }
    let rows = arr
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|c| {
                    row.0
                        .iter()
                        .find(|(k, _)| k == c)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Null)
                })
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// A small RFC 4180 reader: quoted fields, doubled quotes inside them, CRLF.
/// The first row is the header.
fn table_from_csv(raw: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut field = String::new();
    let mut record: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    if in_quotes {
        return Err("CSV ends inside a quoted field (unbalanced `\"`)".into());
    }
    let mut it = records.into_iter().filter(|r| !r.iter().all(|f| f.is_empty()));
    let columns = it.next().ok_or("CSV has no header row")?;
    let width = columns.len();
    let rows: Vec<Vec<Value>> = it
        .map(|r| {
            let mut cells: Vec<Value> = r.into_iter().map(csv_value).collect();
            // Ragged rows are padded rather than rejected: a table with a
            // short last line should still be readable.
            cells.resize(width, Value::Null);
            cells
        })
        .collect();
    Ok((columns, rows))
}

/// Numbers stay numbers so the table can right-align them; everything else is
/// text.
fn csv_value(field: String) -> Value {
    let t = field.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() {
            return json!(f);
        }
    }
    json!(field)
}

fn cmd_chart(args: &Args) -> Result<(), String> {
    let (raw, _) = args.body_file_or_stdin()?;
    let spec: Value =
        serde_json::from_str(&raw).map_err(|e| format!("chart spec is not valid JSON: {e}"))?;
    validate_chart(&spec)?;
    emit(args, json!({ "type": "chart", "spec": spec }))
}

/// Reject the specs that would draw something misleading, here at the door
/// rather than in the pane where the message is easy to miss.
fn validate_chart(spec: &Value) -> Result<(), String> {
    let kind = spec
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("chart spec needs a `type` (line, area, scatter, bar, donut or histogram)")?;
    if !matches!(
        kind,
        "line" | "area" | "scatter" | "bar" | "donut" | "histogram"
    ) {
        return Err(format!(
            "chart type {kind:?} is not one of line, area, scatter, bar, donut, histogram"
        ));
    }
    match kind {
        "donut" => {
            let slices = spec
                .get("slices")
                .and_then(|s| s.as_array())
                .ok_or("a donut chart needs a `slices` array")?;
            if slices.is_empty() {
                return Err("a donut chart needs at least one slice".into());
            }
        }
        "histogram" => {
            let values = spec
                .get("values")
                .and_then(|v| v.as_array())
                .ok_or("a histogram needs a `values` array")?;
            if values.is_empty() {
                return Err("a histogram needs at least one value".into());
            }
        }
        _ => {
            let series = spec
                .get("series")
                .and_then(|s| s.as_array())
                .ok_or_else(|| format!("a {kind} chart needs a `series` array"))?;
            if series.is_empty() {
                return Err(format!("a {kind} chart needs at least one series"));
            }
            // Past eight, the categorical palette stops being separable —
            // drawing them anyway produces a chart nobody can read.
            if series.len() > 8 {
                return Err(format!(
                    "{} series is more than a categorical palette can keep apart (max 8); \
                     split this into several charts",
                    series.len()
                ));
            }
        }
    }
    Ok(())
}

fn cmd_stats(args: &Args) -> Result<(), String> {
    let (raw, _) = args.body_file_or_stdin()?;
    let items: Value =
        serde_json::from_str(&raw).map_err(|e| format!("stats data is not valid JSON: {e}"))?;
    let arr = items
        .as_array()
        .ok_or("stats data must be an array of {label, value, delta?, up_is_good?, spark?}")?;
    if arr.is_empty() {
        return Err("stats needs at least one tile".into());
    }
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("stat {i} is not an object"))?;
        if !obj.contains_key("label") {
            return Err(format!("stat {i} has no `label`"));
        }
        if !obj.contains_key("value") {
            return Err(format!("stat {i} has no `value`"));
        }
    }
    emit(args, json!({ "type": "stats", "items": items }))
}

fn cmd_json(args: &Args) -> Result<(), String> {
    let (raw, path) = args.body_file_or_stdin()?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("not valid JSON: {e}"))?;
    let mut cell = json!({ "type": "json", "value": value });
    let title = args.get("title").map(str::to_string).or_else(|| {
        path.as_deref()
            .and_then(|p| p.file_name())
            .and_then(|f| f.to_str())
            .map(str::to_string)
    });
    if let Some(t) = title {
        cell["title"] = json!(t);
    }
    emit(args, cell)
}

fn cmd_image(args: &Args) -> Result<(), String> {
    let src = args
        .positional
        .first()
        .ok_or("usage: jimctl pad image <path> [--caption C] [--alt A]")?;
    let src = Path::new(src);
    if !src.is_file() {
        return Err(format!("no such image file: {}", src.display()));
    }
    let name = args.notebook();
    valid_name(&name)?;
    // Copy into the notebook's assets so the cell keeps rendering after the
    // original moves — a notebook is meant to outlive the command that wrote
    // it.
    let dir = assets_dir(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let stem = src
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or("image path has no file name")?;
    let dest = dir.join(format!("{}-{stem}", now_ms()));
    std::fs::copy(src, &dest)
        .map_err(|e| format!("copying {} to {}: {e}", src.display(), dest.display()))?;

    let mut cell = json!({ "type": "image", "src": dest.to_string_lossy() });
    if let Some(c) = args.get("caption") {
        cell["caption"] = json!(c);
    }
    if let Some(a) = args.get("alt") {
        cell["alt"] = json!(a);
    }
    emit(args, cell)
}

fn cmd_graph(args: &Args) -> Result<(), String> {
    let (dot, path) = args.body_file_or_stdin()?;
    if !dot.contains('{') {
        return Err("that doesn't look like DOT — expected something like `digraph { a -> b }`".into());
    }
    let mut cell = json!({ "type": "graph", "dot": dot });
    let title = args.get("title").map(str::to_string).or_else(|| {
        path.as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|f| f.to_str())
            .map(str::to_string)
    });
    if let Some(t) = title {
        cell["title"] = json!(t);
    }
    emit(args, cell)
}

fn cmd_rm(args: &Args) -> Result<(), String> {
    let id = args
        .positional
        .first()
        .ok_or("usage: jimctl pad rm <cell-id>")?;
    let name = args.notebook();
    valid_name(&name)?;
    let log = read_log(&name);
    if !existing_ids(&log).iter().any(|i| i == id) {
        return Err(format!(
            "notebook {name:?} has no cell {id:?} (every add prints its id)"
        ));
    }
    append_record(&name, &json!({ "op": "remove", "id": id }))?;
    notify(&name, "remove", id);
    Ok(())
}

fn cmd_clear(args: &Args) -> Result<(), String> {
    let name = args.notebook();
    valid_name(&name)?;
    append_record(&name, &json!({ "op": "clear" }))?;
    notify(&name, "clear", "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_header_and_rows() {
        let (cols, rows) = table_from_csv("a,b\n1,x\n2,y\n").unwrap();
        assert_eq!(cols, vec!["a", "b"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], json!(1));
        assert_eq!(rows[0][1], json!("x"));
    }

    #[test]
    fn csv_quoted_fields_keep_commas_and_quotes() {
        let (_, rows) = table_from_csv("a,b\n\"x, y\",\"she said \"\"hi\"\"\"\n").unwrap();
        assert_eq!(rows[0][0], json!("x, y"));
        assert_eq!(rows[0][1], json!("she said \"hi\""));
    }

    #[test]
    fn csv_ragged_rows_are_padded_not_rejected() {
        let (cols, rows) = table_from_csv("a,b,c\n1,2\n").unwrap();
        assert_eq!(cols.len(), 3);
        assert_eq!(rows[0].len(), 3);
        assert_eq!(rows[0][2], Value::Null);
    }

    #[test]
    fn csv_unbalanced_quote_is_an_error() {
        let err = table_from_csv("a,b\n\"oops,2\n").unwrap_err();
        assert!(err.contains("unbalanced"), "{err}");
    }

    #[test]
    fn json_array_of_objects_unions_keys_in_order() {
        // Document order, not alphabetical: `region` was written first.
        let (cols, rows) =
            table_from_json(r#"[{"region":1,"a":2},{"a":3,"c":4}]"#).unwrap();
        assert_eq!(cols, vec!["region", "a", "c"]);
        assert_eq!(rows[1][0], Value::Null);
        assert_eq!(rows[1][2], json!(4));
        assert_eq!(rows[0][0], json!(1));
    }

    #[test]
    fn json_columns_rows_shape() {
        let (cols, rows) =
            table_from_json(r#"{"columns":["x"],"rows":[[1],[2]]}"#).unwrap();
        assert_eq!(cols, vec!["x"]);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn chart_rejects_unknown_type() {
        let spec: Value = serde_json::from_str(r#"{"type":"sankey"}"#).unwrap();
        let err = validate_chart(&spec).unwrap_err();
        assert!(err.contains("sankey"), "{err}");
    }

    #[test]
    fn chart_rejects_too_many_series() {
        let series: Vec<Value> = (0..9).map(|i| json!({ "name": i.to_string() })).collect();
        let spec = json!({ "type": "line", "series": series });
        let err = validate_chart(&spec).unwrap_err();
        assert!(err.contains("max 8"), "{err}");
    }

    #[test]
    fn chart_accepts_a_normal_line_spec() {
        let spec = json!({ "type": "line", "series": [{ "points": [["a", 1]] }] });
        assert!(validate_chart(&spec).is_ok());
    }

    #[test]
    fn names_must_be_filesystem_and_topic_safe() {
        assert!(valid_name("incident-42").is_ok());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("").is_err());
        assert!(valid_name(".hidden").is_err());
    }

    #[test]
    fn fresh_id_avoids_ids_already_in_the_log() {
        let log = r#"{"op":"update","id":"abcd","cell":{"type":"markdown"}}"#;
        let id = fresh_id(log);
        assert_ne!(id, "abcd");
        assert_eq!(id.len(), 4);
    }
}
