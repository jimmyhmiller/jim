//! `cargo run -p glaze --example check -- sheet.glz [more.glz …]`
//!
//! A linter for Glaze stylesheets: parse each file, then resolve every
//! style in it — at a wide and a narrow viewport, so `when` blocks are
//! exercised too — and print what compiled or exactly why it didn't.
//!
//! Worth having because Glaze failures are easy to write and, until a
//! widget renders, invisible. The property grammar is newline-separated,
//! so `pad 8px radius 8px` on one line parses as a three-argument `pad`
//! and only fails at *resolve* time with "`pad` takes 1, 2, or 4 lengths".
//! Running this after editing a sheet turns that into an instant answer.
//!
//! Exit status is 1 if anything failed to parse or resolve, so it works
//! as a pre-commit / CI gate over a directory of sheets.

use std::collections::HashMap;

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: check <sheet.glz> [more.glz …]");
        std::process::exit(2);
    }

    let mut failed = false;
    for file in &files {
        println!("{file}");
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                println!("  cannot read: {e}");
                failed = true;
                continue;
            }
        };
        let program = match glaze::parse(&src) {
            Ok(p) => p,
            Err(e) => {
                println!("  parse error: {e}");
                failed = true;
                continue;
            }
        };
        println!(
            "  parsed: {} tokens, {} fns, {} styles",
            program.tokens.len(),
            program.fns.len(),
            program.styles.len()
        );

        // Resolve each style at a wide and a narrow viewport. A style with
        // required variant params may legitimately fail without them, so
        // report rather than assume — the message names the missing input.
        let empty: HashMap<String, String> = HashMap::new();
        for style in &program.styles {
            for (label, vw) in [("wide", 1200.0), ("narrow", 320.0)] {
                match program.resolve_at(&style.name, &empty, &[], vw, 800.0) {
                    Ok(c) => println!(
                        "  {} @{}: {} layer(s), radius {}, pad {:?}",
                        style.name,
                        label,
                        c.layers.len(),
                        c.box_.radius,
                        c.box_.padding
                    ),
                    Err(e) => {
                        println!("  {} @{}: {e}", style.name, label);
                        failed = true;
                    }
                }
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}
