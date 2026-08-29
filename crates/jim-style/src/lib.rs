//! Per-project styling — design tokens driven by user-edited files on
//! disk.
//!
//! # Architecture
//!
//! Everything visual is **data on disk**, not compiled-in Rust types:
//! per-project `theme.ft` token files and named presets under
//! `~/.jim/styles/`, hot-reloaded by notify watchers. The runtime is a
//! small fixed surface: the [`Theme`] resource (active project),
//! [`ProjectThemes`] (per-project cache so every pane can render in its
//! OWN project's look), the preset registry, chrome-theme glue, and the
//! funct host-fn bridges for theme editing and color math.
//!
//! (The old dynamic canvas-shader pipeline — the fullscreen "dust"
//! overlay with its script bridge and WGSL introspection — was removed;
//! per-preset chrome shaders in `jim-pane` are unaffected.)

use bevy::prelude::*;

pub mod active;
pub mod chrome_theme;
pub mod fonts;
pub mod material;
pub mod oklab;
pub mod presets;
pub mod script_bridge;
pub mod state;
pub mod theme;
pub mod theme_bridge;

pub use active::ActiveProject;
pub use fonts::{FontRegistry, FontRegistryPlugin};
pub use material::{register_preset_asset_source, register_style_asset_source};
pub use presets::{
    ActiveStylePreset, PresetsPlugin, StylePreset, StylePresetRegistry,
    register_preset_host_fns_funct, resolve_project_theme,
};
pub use script_bridge::register_script_host_fns_funct;
pub use state::{ProjectStyleState, StyleDataDir};
pub use theme::{ProjectThemes, Theme, ThemeChanged, TokenId, TokenValue, tokens};
pub use theme_bridge::{ThemeBridgePlugin, register_theme_host_fns_funct};

// Compatibility re-exports for hosts that still spell paths the old
// way. Once terminal-bevy switches to `jim_style::ActiveProject`,
// these can go.
pub mod shader {
    pub use crate::active::ActiveProject;
}

/// Errors from theme parsing surfaced as a resource so a status pane
/// can show them. Never crashes the host — broken files just keep
/// the last good version.
#[derive(Resource, Default, Debug, Clone)]
pub struct StyleErrors {
    pub theme_error: Option<String>,
}

/// Top-level plugin: the theme system (tokens, presets, per-project
/// cache, chrome glue, fonts, editing bridge).
pub struct StylePlugin;

impl Plugin for StylePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Theme>()
            .init_resource::<ProjectStyleState>()
            .init_resource::<theme::ProjectThemes>()
            .init_resource::<StyleErrors>()
            .init_resource::<ActiveProject>()
            .add_message::<ThemeChanged>()
            .add_plugins(theme::ThemePlugin)
            .add_plugins(FontRegistryPlugin)
            .add_plugins(chrome_theme::ChromeThemePlugin)
            .add_plugins(PresetsPlugin)
            .add_plugins(ThemeBridgePlugin);
    }
}
