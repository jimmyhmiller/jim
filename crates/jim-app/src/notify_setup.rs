//! Idempotent notification setup for Codex.
//!
//! Codex's TUI can emit a literal BEL for `agent-turn-complete`. Jim's
//! terminal worker already turns that byte into the per-project unread pulse,
//! so this is preferable to adding a second Codex-specific event path.

use std::path::PathBuf;

use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// Merge Codex's turn-complete BEL settings into `~/.codex/config.toml`.
/// Existing keys, comments, and the unrelated external `notify` command are
/// preserved. The latter matters because the Codex desktop app may own it.
pub(super) fn install_codex_notify() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME env".to_string())?;
    let path = PathBuf::from(home).join(".codex/config.toml");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
    }

    let existed = path.exists();
    let raw = if existed {
        std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?
    } else {
        String::new()
    };
    let out = merge_codex_bell(&raw)?;

    let mut backup_note = String::new();
    if existed && out != raw {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = path.with_extension(format!("toml.bak.{ts}"));
        if std::fs::write(&backup, &raw).is_ok() {
            backup_note = format!(" (backup: {})", backup.display());
        }
    }

    if out != raw {
        std::fs::write(&path, out).map_err(|e| format!("write {path:?}: {e}"))?;
    }

    Ok(format!(
        "Installed agent-turn-complete BEL into {}{}. Restart Codex for the setting to take effect.",
        path.display(),
        backup_note
    ))
}

fn merge_codex_bell(raw: &str) -> Result<String, String> {
    let mut doc = raw
        .parse::<DocumentMut>()
        .map_err(|e| format!("parse config.toml: {e}"))?;

    let tui = doc
        .entry("tui")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "config.toml `tui` is not a table".to_string())?;

    let mut events = Array::new();
    events.push("agent-turn-complete");
    tui["notifications"] = Item::Value(Value::Array(events));
    tui["notification_method"] = toml_edit::value("bel");
    // Jim performs its own visibility check before adding an unread badge.
    // Always emitting here ensures Jim sees the event even while its window is
    // technically focused but a different project is active.
    tui["notification_condition"] = toml_edit::value("always");

    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::merge_codex_bell;

    #[test]
    fn adds_bell_without_clobbering_existing_codex_settings() {
        let input = r#"model = "gpt-5.6-sol"
notify = ["existing-notifier", "turn-ended"]

[tui.model_availability_nux]
"gpt-5.6-sol" = 4
"#;
        let output = merge_codex_bell(input).unwrap();
        let parsed = output.parse::<toml_edit::DocumentMut>().unwrap();

        assert_eq!(parsed["model"].as_str(), Some("gpt-5.6-sol"));
        assert_eq!(parsed["notify"][0].as_str(), Some("existing-notifier"));
        assert_eq!(
            parsed["tui"]["notifications"][0].as_str(),
            Some("agent-turn-complete")
        );
        assert_eq!(parsed["tui"]["notification_method"].as_str(), Some("bel"));
        assert_eq!(
            parsed["tui"]["notification_condition"].as_str(),
            Some("always")
        );
        assert_eq!(
            parsed["tui"]["model_availability_nux"]["gpt-5.6-sol"].as_integer(),
            Some(4)
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let once = merge_codex_bell("").unwrap();
        let twice = merge_codex_bell(&once).unwrap();
        assert_eq!(once, twice);
    }
}
