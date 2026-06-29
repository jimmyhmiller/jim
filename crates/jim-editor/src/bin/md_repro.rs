//! Standalone, deterministic reproduction of the markdown WYSIWYG
//! overlapping-lines bug — renders a wrapping markdown doc in the
//! self-contained editor (its OWN camera; no canvas/cube/visibility
//! namespacing) and saves a PNG so we can inspect the ACTUAL render
//! instead of the (correct-looking) layout model.
//!
//! Run: `cargo run -p jim_editor --bin md_repro`
//! Output: `target/screenshots/md_repro.png`

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::window::PrimaryWindow;
use jim_editor::{spawn_editor_pane, EditorFilePath, EditorPlugin, EditorStateComp};
use jim_pane::{FocusedPane, PaneRect};
use editor_core::commands::insert_newline_and_indent;
use editor_core::selection::Selection;
use editor_core::transaction::Transaction;

const DOC: &str = "\
# Is All Vibe Coding Slop?

I have to admit, I quite enjoy vibe coding. I mean this in the truest sense, using Claude to generate code I never look at. I know that if I looked at the code I generated, I'd have a million changes I'd want to make to it. I know it is full of special cases that need not be special cases. I know that it is code that certainly wouldn't.

pass a code review. But often, the concrete output is something incredibly useful for me.

To be concrete, I'll consider a little utility I vibe coded but use every day keep-running. Keep Running is just my version of dtach. A program I could never remember how to use properly. So far in my use of it, I haven't encountered errors, I've never had a session drop on me.

## What Makes Something Slop?

I am certain some people have an overly broad conception of slop that entails that all ai generated anything is slop. I won't give that kind of view the time of day here. But I do think it is far from clear what exactly slop is. One way of thinking about slop makes us consider it as a quality measurement.

## Human Written Slop Code

I did once encounter what I consider a definitive example of slop code well before AI. At the time I worked for a company that had a simple take home assignment. You had to build a rest api and a single page app frontend. We had one candidate with twenty plus years of java experience.

## Another Heading

Short line one.
Short line two.
";

#[derive(Resource)]
struct Driver {
    frame: u32,
    spawned: bool,
    shot: bool,
}

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "md_repro".into(),
            resolution: (900u32, 700u32).into(),
            ..default()
        }),
        ..default()
    }));
    // The standalone EditorPlugin relies on jim_style's Theme / FontRegistry
    // (setup_editor_font + markdown fonts read them).
    app.add_plugins(jim_style::StylePlugin);
    app.add_plugins(EditorPlugin);
    app.insert_resource(Driver {
        frame: 0,
        spawned: false,
        shot: false,
    });
    app.add_systems(Update, drive);
    app.run();
}

fn drive(world: &mut World) {
    let frame = {
        let mut d = world.resource_mut::<Driver>();
        d.frame += 1;
        d.frame
    };

    // Spawn the editor on frame 1 — by now all Startup commands (PaneFont,
    // camera, fonts) are flushed, so spawn_pane has what it needs.
    if !world.resource::<Driver>().spawned {
        let e = spawn_editor_pane(
            world,
            DOC,
            PaneRect {
                pos: Vec2::new(40.0, 40.0),
                size: Vec2::new(820.0, 600.0),
                z: 1.0,
            },
            None,
        );
        // .md path → detect_markdown_mode flips on WYSIWYG.
        world
            .entity_mut(e)
            .insert(EditorFilePath(PathBuf::from("repro.md")));
        // Focus it so the text lays out (the user's repro is a focused pane).
        world.resource_mut::<FocusedPane>().0 = Some(e);
        world.resource_mut::<Driver>().spawned = true;
        return;
    }

    // Frame 40: insert a blank line right BEFORE the "I have to admit"
    // paragraph — exactly what the user does ("I put a new line before it").
    // This reuses pool entities for the shifted lines, which is what surfaces
    // the stale-glyph duplication.
    if frame == 40 {
        let mut q = world.query::<(Entity, &mut EditorStateComp)>();
        if let Some((_e, mut state)) = q.iter_mut(world).next() {
            let doc = state.0.doc.to_string();
            if let Some(off) = doc.find("I have to admit") {
                // place caret at paragraph start, then insert a newline
                state.0 = state
                    .0
                    .apply(&Transaction::new().select(Selection::cursor(off)));
                if let Some(tr) = insert_newline_and_indent(&state.0) {
                    state.0 = state.0.apply_with_history(&tr);
                    eprintln!("[md_repro] inserted newline before paragraph at off={off}");
                }
            }
        }
        return;
    }

    // Capture after the text pipeline + readback have had many frames.
    if frame == 90 && !world.resource::<Driver>().shot {
        let has_window = world
            .query_filtered::<Entity, With<PrimaryWindow>>()
            .iter(world)
            .next()
            .is_some();
        if has_window {
            let dir = workspace_root().join("target/screenshots");
            std::fs::create_dir_all(&dir).ok();
            let path = dir.join("md_repro.png");
            eprintln!("→ {}", path.display());
            world
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path));
            world.resource_mut::<Driver>().shot = true;
        }
    }

    if frame >= 110 {
        world
            .resource_mut::<Messages<AppExit>>()
            .write(AppExit::Success);
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}
