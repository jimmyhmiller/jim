//! `style-muse` — CLI face of the Style Lab generator. One JSON line on
//! stdout per invocation; the `style_lab.rhai` widget drives it over
//! `proc_spawn` / `on_proc_output`.
//!
//!     style-muse next <count> <random|llm|auto> [note...]
//!     style-muse feedback '<feedback-event-json>'
//!     style-muse adopt '<genome-json>' <preset-name>

use inference_bevy::muse;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = match args.first().map(|s| s.as_str()) {
        Some("next") => {
            let count: usize = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(4)
                .clamp(1, 12);
            let mode = args.get(2).map(|s| s.as_str()).unwrap_or("auto").to_string();
            let note = args.get(3..).map(|s| s.join(" ")).unwrap_or_default();
            muse::next_batch(count, &mode, &note)
        }
        Some("feedback") => {
            let Some(raw) = args.get(1) else {
                fail("feedback requires a JSON event argument");
            };
            match serde_json::from_str::<muse::FeedbackEvent>(raw) {
                Ok(ev) => match muse::append_feedback(ev) {
                    Ok(()) => serde_json::json!({"ok": true}),
                    Err(e) => serde_json::json!({"error": format!("write feedback: {e}")}),
                },
                Err(e) => serde_json::json!({"error": format!("bad feedback JSON: {e}")}),
            }
        }
        Some("adopt") => {
            let (Some(raw), Some(preset)) = (args.get(1), args.get(2)) else {
                fail("adopt requires <genome-json> <preset-name>");
            };
            match serde_json::from_str::<muse::Genome>(raw) {
                Ok(mut g) => {
                    let mut rng = muse::Rng::from_clock();
                    g.sanitize(&mut rng);
                    match muse::adopt(&g, preset) {
                        Ok(dir) => serde_json::json!({
                            "ok": true,
                            "preset": preset,
                            "dir": dir.display().to_string(),
                        }),
                        Err(e) => serde_json::json!({"error": format!("write preset: {e}")}),
                    }
                }
                Err(e) => serde_json::json!({"error": format!("bad genome JSON: {e}")}),
            }
        }
        _ => serde_json::json!({
            "error": "usage: style-muse next <count> <random|llm|auto> [note] | feedback <json> | adopt <genome-json> <preset>"
        }),
    };
    println!("{}", out);
}

fn fail(msg: &str) -> ! {
    println!("{}", serde_json::json!({ "error": msg }));
    std::process::exit(1);
}
