//! Which transcription engine dictation uses, switchable at runtime.
//!
//! The choice lives in `~/.jim/dictation.json` (`{"engine": "phonon"}`) and
//! is read when each dictation STARTS, so a switch never disturbs one in
//! flight, and anything that can write a file — an agent, a script — can
//! flip it. The palette's "Dictation: Use Phonon / Use Whisper" actions write
//! it and warm the new engine. A missing file means Whisper, the long-time
//! default; a file that can't be read or parsed is an error, never a silent
//! fallback.
//!
//! Switching does not stop the other engine's server. Both are shared
//! services that outlive jim (see `server.rs`), and keeping the previous one
//! warm is what makes switching back instant.

use std::path::PathBuf;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::{Dictation, Transcriber, phonon, whisper};
use crate::actions::{Action, ActionCtx, ActionRun};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Whisper,
    Phonon,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Whisper => "Whisper",
            Engine::Phonon => "Phonon",
        }
    }

    pub fn prewarm(self) {
        match self {
            Engine::Whisper => whisper::prewarm(),
            Engine::Phonon => phonon::prewarm(),
        }
    }

    /// Bring the engine up and open a session. Blocking — the dictation
    /// worker calls it before the microphone opens.
    pub fn start(self) -> Result<Box<dyn Transcriber>, String> {
        match self {
            Engine::Whisper => whisper::start(),
            Engine::Phonon => phonon::start(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Settings {
    engine: Engine,
}

fn settings_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".jim/dictation.json"))
}

/// The engine the next dictation will use.
pub fn selected() -> Result<Engine, String> {
    let path = settings_path().ok_or("no HOME")?;
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Engine::default()),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
    }
}

fn parse(text: &str) -> Result<Engine, String> {
    serde_json::from_str::<Settings>(text)
        .map(|s| s.engine)
        .map_err(|e| format!("expected {{\"engine\": \"whisper\" | \"phonon\"}}: {e}"))
}

pub fn select(engine: Engine) -> Result<(), String> {
    let path = settings_path().ok_or("no HOME")?;
    let body = serde_json::to_string_pretty(&Settings { engine }).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body + "\n")
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn switch(world: &mut World, engine: Engine) {
    let now = world.resource::<Time>().elapsed_secs_f64();
    match select(engine) {
        Ok(()) => {
            eprintln!("[dictation] engine switched to {}", engine.label());
            engine.prewarm();
            let busy = world.resource::<Dictation>().is_active();
            world.resource_mut::<Dictation>().notice(
                if engine == Engine::Phonon && !phonon::installed() {
                    "Installing Phonon (first use, a minute or two)…".into()
                } else if busy {
                    format!("Dictation will use {} from the next recording", engine.label())
                } else {
                    format!("Dictation now uses {}", engine.label())
                },
                now,
            );
        }
        Err(e) => world.resource_mut::<Dictation>().set_error(e, now),
    }
}

pub const USE_PHONON: Action = Action {
    id: "dictation.engine.phonon",
    title: "Dictation: Use Phonon (fast, English-only)",
    category: "Dictation",
    keywords: &["dictation", "speech", "voice", "transcribe", "engine", "phonon", "parakeet"],
    radial_icon: None,
    default_keys: &[],
    run: ActionRun::Custom(|ctx: &mut ActionCtx| switch(ctx.world, Engine::Phonon)),
};

pub const USE_WHISPER: Action = Action {
    id: "dictation.engine.whisper",
    title: "Dictation: Use Whisper",
    category: "Dictation",
    keywords: &["dictation", "speech", "voice", "transcribe", "engine", "whisper"],
    radial_icon: None,
    default_keys: &[],
    run: ActionRun::Custom(|ctx: &mut ActionCtx| switch(ctx.world, Engine::Whisper)),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_engines() {
        assert_eq!(parse(r#"{"engine":"phonon"}"#), Ok(Engine::Phonon));
        assert_eq!(parse(r#"{"engine": "whisper"}"#), Ok(Engine::Whisper));
    }

    /// A typo must be an error the user sees, not a quiet fall back to
    /// whichever engine they were trying to leave.
    #[test]
    fn rejects_an_unknown_engine() {
        let e = parse(r#"{"engine":"phonnon"}"#).unwrap_err();
        assert!(e.contains("whisper") && e.contains("phonon"), "{e}");
        assert!(parse("{}").is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn round_trips_through_json() {
        for e in [Engine::Whisper, Engine::Phonon] {
            let s = serde_json::to_string(&Settings { engine: e }).unwrap();
            assert_eq!(parse(&s), Ok(e));
        }
    }
}
