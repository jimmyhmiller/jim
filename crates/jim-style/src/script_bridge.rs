//! Color-math host fns for funct widget workers.
//!
//! This module once carried the whole dynamic-canvas script bridge
//! (`uniform_set` / `mask_paint` / per-project script state / an event
//! bus) that drove the dust overlay. That system is gone; what remains
//! is the small set of pure color helpers widgets actually use
//! (`theme_editor.ft` and friends).

use bevy::prelude::*;

/// Register the color-helper host fns. Call LAST in the worker's
/// registration order: the funct engine ships a formatting `oklch`
/// (returns an `oklch(...)` CSS string) and this hex-returning one
/// must override it for widget color math.
pub fn register_script_host_fns_funct(vm: &mut funct::Funct) {
    // ---- OkLCh / OkLab color helpers (hex, matching the funct surface) ----
    vm.register3("oklch", |l: f64, c: f64, h: f64| -> String {
        linear_rgba_to_hex(crate::oklab::oklch_to_linear_srgb(
            l as f32, c as f32, h as f32,
        ))
    });
    vm.register3("oklab", |l: f64, a: f64, b: f64| -> String {
        linear_rgba_to_hex(crate::oklab::oklab_to_linear_srgb(
            l as f32, a as f32, b as f32,
        ))
    });

    // ---- theme_contrast(a, b) → OkLab L difference [0, 100] ----
    vm.register2("theme_contrast", |a: String, b: String| -> f64 {
        let (Ok(ca), Ok(cb)) = (
            crate::theme::parse_color_string(&a),
            crate::theme::parse_color_string(&b),
        ) else {
            return 0.0;
        };
        crate::oklab::lightness_delta(ca, cb) as f64
    });
}

/// Format a `LinearRgba` as `#rrggbbaa` (alpha appended only when not 1).
fn linear_rgba_to_hex(c: bevy::color::LinearRgba) -> String {
    use bevy::color::Color;
    let srgb = Color::LinearRgba(c).to_srgba();
    let r = (srgb.red.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (srgb.green.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (srgb.blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = (srgb.alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    if a == 255 {
        format!("#{:02x}{:02x}{:02x}", r, g, b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", r, g, b, a)
    }
}
