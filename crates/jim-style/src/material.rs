//! Asset source registration for the `preset://` URL scheme, plus the
//! [`StyleDataDir`] bootstrap.
//!
//! Once upon a time this module also held a typed `BackgroundMaterial`
//! and later a `style://` asset source feeding the dynamic canvas
//! ("dust") overlay; both are gone. What survives: `preset://` for
//! per-preset chrome shaders, and the data-dir resource everything
//! per-project (themes, state.json) hangs off.

use std::path::PathBuf;

use bevy::asset::io::{AssetSourceBuilder, AssetSourceId};
use bevy::prelude::*;

use crate::state::StyleDataDir;

/// Asset source rooted at `~/.jim/styles/`. Per-preset
/// shaders are loaded via `preset://<name>/chrome.wgsl`. Registered
/// by [`register_preset_asset_source`].
pub const PRESET_SOURCE: &str = "preset";

/// Register the `preset://` asset source. Must be called BEFORE
/// `DefaultPlugins` (same constraint as [`register_style_asset_source`]).
pub fn register_preset_asset_source(app: &mut App, base_dir: PathBuf) {
    if !base_dir.exists()
        && let Err(e) = std::fs::create_dir_all(&base_dir)
    {
        eprintln!(
            "[style] failed to create preset base dir {:?}: {} — preset shaders will be unavailable",
            base_dir, e
        );
        return;
    }
    let path_str = match base_dir.to_str() {
        Some(s) => s.to_string(),
        None => {
            eprintln!("[style] preset base dir is not utf-8: {:?}", base_dir);
            return;
        }
    };
    app.register_asset_source(
        AssetSourceId::Name(PRESET_SOURCE.into()),
        AssetSourceBuilder::platform_default(&path_str, None),
    );
}

/// Ensure the per-project style data dir exists and publish it as
/// [`StyleDataDir`] so themes / presets / state.json can find it.
///
/// (This used to also register a hot-reloading `style://` asset source
/// over the whole projects tree for the dust overlay's shader — removed
/// with that system, which also drops its recursive file watcher.)
pub fn register_style_asset_source(app: &mut App, base_dir: PathBuf) {
    if !base_dir.exists()
        && let Err(e) = std::fs::create_dir_all(&base_dir)
    {
        eprintln!(
            "[style] failed to create style base dir {:?}: {} — per-project themes will be unavailable",
            base_dir, e
        );
        return;
    }

    // Insert the data dir as a resource so other systems can pick it up.
    app.insert_resource(StyleDataDir(base_dir));
}
