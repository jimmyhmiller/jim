//! Glaze ⇄ funct bridge — the host natives that let a `.ft` widget style
//! itself with the Glaze style language instead of hand-built `Style`
//! records.
//!
//! Before this module, Glaze was a Rust-only API: `glaze::parse` →
//! [`glaze::Program::resolve`] → [`crate::glaze_style::to_style`]. The
//! only consumers were Rust binaries that embedded their sheet as a
//! `const` (`bin/glaze_ui.rs`). Scripts had no way in, so every widget
//! rebuilt the same inline `Style` maps by hand — the JSSS problem
//! `docs/GLAZE.md` set out to kill.
//!
//! The seam is small because [`crate::glaze_style::to_style`] already
//! returns a [`protocol::Style`], which is exactly the record shape the
//! renderer decodes out of a widget's frame. So the whole bridge is:
//!
//! ```text
//! .glz source ──parse──► Program ──resolve(name, variant, states)──►
//!     CompiledStyle ──to_style──► protocol::Style ──serde──► funct Value
//! ```
//!
//! and the widget just drops that value under `style:`:
//!
//! ```funct
//! glaze_load_file(widget_asset("talk.glz"))
//! el("frame", { style: glaze("card"), children: [ … ] })
//! ```
//!
//! Shader layers come through for free: `to_style` lowers `Layer::Shader`
//! to [`protocol::GlazeLayer::Shader`], which `render.rs` already turns
//! into a live `DynamicMaterial`. A funct widget therefore gets animated
//! WGSL overlays without touching the GPU path.
//!
//! ## One sheet per widget
//!
//! Each widget worker owns its own [`GlazeSheet`]. Loading twice replaces
//! the sheet (that is what hot reload does). Sheets are not shared between
//! widgets — a shared design system is a `.glz` file both widgets load,
//! not a global the host mutates behind their backs.
//!
//! ## Loud, never silent
//!
//! House rule from `docs/GLAZE.md`: a parse error, an unknown token, an
//! unknown style name, or an unknown component slot raises a funct
//! [`Fault`] carrying the Glaze message. It never returns a default style
//! — an unstyled element that silently "works" is exactly the kind of
//! failure that costs an hour to trace back to a typo'd style name.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use funct::{Fault, Funct, Value};
use glaze::{CompiledStyle, Program};

use crate::glaze_style;

/// A widget's loaded stylesheet, shared between the funct natives (which
/// write it on `glaze_load*`) and the main thread (which reads `path` to
/// decide whether a changed `.glz` file should reload this widget).
#[derive(Clone, Default)]
pub(crate) struct GlazeSheet(Arc<Mutex<GlazeSheetInner>>);

#[derive(Default)]
struct GlazeSheetInner {
    program: Option<Program>,
    /// Canonical path the sheet came from, or `None` for a sheet compiled
    /// from literal source. Drives hot reload.
    path: Option<PathBuf>,
}

impl GlazeSheet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The canonical path of the loaded sheet, if it came from a file.
    /// Read by `poll_watcher` on the main thread.
    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.0.lock().ok().and_then(|g| g.path.clone())
    }

    fn set(&self, program: Program, path: Option<PathBuf>) {
        if let Ok(mut g) = self.0.lock() {
            g.program = Some(program);
            g.path = path;
        }
    }

    /// Run `f` against the loaded program, or fault if none is loaded.
    fn with<T>(&self, who: &str, f: impl FnOnce(&Program) -> Result<T, Fault>) -> Result<T, Fault> {
        let g = self
            .0
            .lock()
            .map_err(|_| Fault::new("glaze sheet lock poisoned"))?;
        match g.program.as_ref() {
            Some(p) => f(p),
            None => Err(Fault::new(format!(
                "{who}: no stylesheet loaded — call glaze_load(src) or \
                 glaze_load_file(path) first"
            ))),
        }
    }
}

// ---------------------------------------------------------------- args

fn arg_str(args: &[Value], i: usize, who: &str, what: &str) -> Result<String, Fault> {
    match args.get(i) {
        Some(Value::Str(s)) => Ok(s.to_string()),
        _ => Err(Fault::new(format!("{who} expects {what} as a string"))),
    }
}

/// A funct value used as a Glaze *variant* map. Glaze variant params are
/// compared as strings (`intent == danger`), so scalars are stringified
/// rather than rejected — `{ level: 2 }` and `{ dark: true }` both work.
fn arg_variant(args: &[Value], i: usize, who: &str) -> Result<HashMap<String, String>, Fault> {
    let Some(v) = args.get(i) else {
        return Ok(HashMap::new());
    };
    if matches!(v, Value::Unit) {
        return Ok(HashMap::new());
    }
    let json = v.to_json()?;
    let obj = json.as_object().ok_or_else(|| {
        Fault::new(format!(
            "{who}: variant must be a record like {{ intent: \"danger\" }}"
        ))
    })?;
    let mut out = HashMap::with_capacity(obj.len());
    for (k, val) in obj {
        let s = match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => {
                return Err(Fault::new(format!(
                    "{who}: variant field `{k}` must be a string, number, or bool"
                )));
            }
        };
        out.insert(k.clone(), s);
    }
    Ok(out)
}

/// A funct value used as the discrete-state list (`["hover"]`). A bare
/// string is accepted as a one-element list.
fn arg_states(args: &[Value], i: usize, who: &str) -> Result<Vec<String>, Fault> {
    let Some(v) = args.get(i) else {
        return Ok(Vec::new());
    };
    match v {
        Value::Unit => Ok(Vec::new()),
        Value::Str(s) => Ok(vec![s.to_string()]),
        _ => {
            let json = v.to_json()?;
            let arr = json.as_array().ok_or_else(|| {
                Fault::new(format!(
                    "{who}: states must be a list of strings like [\"hover\"]"
                ))
            })?;
            arr.iter()
                .map(|s| {
                    s.as_str()
                        .map(String::from)
                        .ok_or_else(|| Fault::new(format!("{who}: every state must be a string")))
                })
                .collect()
        }
    }
}

fn arg_f32(args: &[Value], i: usize, who: &str, what: &str) -> Result<f32, Fault> {
    match args.get(i) {
        Some(Value::Float(f)) => Ok(*f as f32),
        Some(Value::Int(n)) => Ok(*n as f32),
        _ => Err(Fault::new(format!("{who} expects {what} as a number"))),
    }
}

/// Serialize anything serde-serializable into a funct value.
fn to_value<T: serde::Serialize>(v: &T, who: &str) -> Result<Value, Fault> {
    let json = serde_json::to_value(v)
        .map_err(|e| Fault::new(format!("{who}: could not serialize style: {e}")))?;
    Ok(Value::from_json(&json))
}

use crate::funct_widget::expand_tilde;

// ------------------------------------------------------------- resolve

fn resolve(
    prog: &Program,
    who: &str,
    name: &str,
    variant: &HashMap<String, String>,
    states: &[String],
    size: Option<(f32, f32)>,
) -> Result<CompiledStyle, Fault> {
    let states: Vec<&str> = states.iter().map(String::as_str).collect();
    let r = match size {
        Some((vw, vh)) => prog.resolve_at(name, variant, &states, vw, vh),
        None => prog.resolve(name, variant, &states),
    };
    r.map_err(|e| Fault::new(format!("{who}(\"{name}\"): {e}")))
}

/// Every component `glaze_slot` knows how to style, for the error message.
const COMPONENTS: &str = "toggle, select, tabs, bar, stepper, radio, checkbox, \
                          slider, table, toast, popover, dialog, tooltip";

/// The component-slot resolvers, keyed by the `Element` kind whose typed
/// style struct they produce. Kept as one dispatch so `glaze_slot` reads
/// like the element vocabulary it mirrors.
///
/// `toggle` / `select` / `tabs` resolve themselves from the whole
/// `Program`: their appearance spans several *state* plans (a toggle's
/// track differs checked vs unchecked), so they re-resolve internally
/// rather than reading one already-compiled plan. The rest are pure
/// functions of this style's `part {}` slots.
fn slot_style(
    prog: &Program,
    name: &str,
    component: &str,
    variant: &HashMap<String, String>,
    states: &[String],
) -> Result<Value, Fault> {
    let who = "glaze_slot";
    let err = |e: String| Fault::new(format!("{who}(\"{name}\", \"{component}\"): {e}"));
    match component {
        "toggle" => {
            return to_value(
                &glaze_style::resolve_toggle_style(prog, name).map_err(err)?,
                who,
            );
        }
        "select" => {
            return to_value(
                &glaze_style::resolve_select_style(prog, name).map_err(err)?,
                who,
            );
        }
        "tabs" => {
            return to_value(
                &glaze_style::resolve_tabs_style(prog, name).map_err(err)?,
                who,
            );
        }
        _ => {}
    }

    let state_refs: Vec<&str> = states.iter().map(String::as_str).collect();
    let slots = prog
        .resolve_slots(name, variant, &state_refs)
        .map_err(|e| Fault::new(format!("{who}(\"{name}\", \"{component}\"): {e}")))?;

    match component {
        "bar" => to_value(&glaze_style::to_bar_style(&slots).map_err(err)?, who),
        "stepper" => to_value(&glaze_style::to_stepper_style(&slots).map_err(err)?, who),
        "radio" => to_value(&glaze_style::to_radio_style(&slots).map_err(err)?, who),
        "checkbox" => to_value(&glaze_style::to_checkbox_style(&slots).map_err(err)?, who),
        "slider" => to_value(&glaze_style::to_slider_style(&slots).map_err(err)?, who),
        "table" => to_value(&glaze_style::to_table_style(&slots).map_err(err)?, who),
        "toast" => to_value(&glaze_style::to_toast_style(&slots).map_err(err)?, who),
        "popover" => to_value(&glaze_style::to_popover_style(&slots).map_err(err)?, who),
        "dialog" => to_value(&glaze_style::to_dialog_style(&slots).map_err(err)?, who),
        "tooltip" => to_value(&glaze_style::to_tooltip_style(&slots).map_err(err)?, who),
        other => Err(Fault::new(format!(
            "glaze_slot: unknown component `{other}` (known: {COMPONENTS})"
        ))),
    }
}

// ------------------------------------------------------------ register

/// Register the `glaze_*` natives on a widget's funct VM. `sheet` is the
/// worker's own stylesheet slot (see [`GlazeSheet`]).
pub(crate) fn register(vm: &mut Funct, sheet: &GlazeSheet) {
    // glaze_load(src) -> true. Compile literal Glaze source. Faults with
    // the compiler's message on a parse error.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_load", move |_vm, args| {
            let src = arg_str(&args, 0, "glaze_load", "Glaze source")?;
            let prog = glaze::parse(&src).map_err(|e| Fault::new(format!("glaze_load: {e}")))?;
            sheet.set(prog, None);
            Ok(Value::Bool(true))
        });
    }

    // glaze_load_file(path) -> true. Read + compile. `~` is expanded and
    // the path is canonicalized so the file watcher's (symlink-resolved)
    // events match it — `~/.jim/widgets` is itself a symlink, so without
    // this hot reload would silently never fire.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_load_file", move |_vm, args| {
            let raw = arg_str(&args, 0, "glaze_load_file", "a path")?;
            let path = PathBuf::from(expand_tilde(&raw));
            let src = std::fs::read_to_string(&path)
                .map_err(|e| Fault::new(format!("glaze_load_file(\"{}\"): {e}", path.display())))?;
            let prog = glaze::parse(&src)
                .map_err(|e| Fault::new(format!("glaze_load_file(\"{}\"): {e}", path.display())))?;
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            sheet.set(prog, Some(canonical));
            Ok(Value::Bool(true))
        });
    }

    // glaze_loaded() -> bool. For a widget that wants to fall back to
    // inline styling rather than fault when no sheet is present.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_loaded", move |_vm, _args| {
            let loaded = sheet.0.lock().map(|g| g.program.is_some()).unwrap_or(false);
            Ok(Value::Bool(loaded))
        });
    }

    // glaze_styles() -> [name]. Introspection, for style pickers and for
    // "did I typo the name" debugging.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_styles", move |_vm, _args| {
            sheet.with("glaze_styles", |p| {
                Ok(Value::list(
                    p.styles
                        .iter()
                        .map(|s| Value::str(s.name.clone()))
                        .collect(),
                ))
            })
        });
    }

    // glaze_token(name) -> the token's value: a color as "#rrggbb[aa]",
    // a length/number as a number, a bool, or a bare symbol as a string.
    //
    // Styles carry BOX properties only, so a font size or a text colour
    // can't come out of `glaze(...)`. Reading the token keeps typography
    // in the sheet with everything else, instead of splitting a design
    // system across a `.glz` and a pile of literals in the `.ft`.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_token", move |_vm, args| {
            let who = "glaze_token";
            let name = arg_str(&args, 0, who, "a token name")?;
            sheet.with(who, |p| {
                let v = p
                    .token(&name)
                    .map_err(|e| Fault::new(format!("{who}(\"{name}\"): {e}")))?;
                Ok(match v {
                    glaze::Value::Num(n) => Value::Float(n),
                    glaze::Value::Bool(b) => Value::Bool(b),
                    glaze::Value::Color(c) => Value::str(glaze_style::hex(c)),
                    glaze::Value::Sym(s) => Value::str(s),
                    // Lengths are px in practice; `%`/`em` have no numeric
                    // meaning without a containing box, so they come back as
                    // the string a `Style` field would accept.
                    glaze::Value::Len(l) => match l {
                        glaze::Length::Px(px) => Value::Float(px as f64),
                        glaze::Length::Pct(p) => Value::str(format!("{p}%")),
                        glaze::Length::Em(e) => Value::str(format!("{e}em")),
                        glaze::Length::Auto => Value::str("auto"),
                    },
                })
            })
        });
    }

    // glaze_tokens() -> [name]. Introspection, like glaze_styles().
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_tokens", move |_vm, _args| {
            sheet.with("glaze_tokens", |p| {
                Ok(Value::list(
                    p.token_names().into_iter().map(Value::str).collect(),
                ))
            })
        });
    }

    // glaze(name) / glaze(name, variant) / glaze(name, variant, states)
    //   -> a Style record, ready to drop under an element's `style:`.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze", move |_vm, args| {
            let who = "glaze";
            let name = arg_str(&args, 0, who, "a style name")?;
            let variant = arg_variant(&args, 1, who)?;
            let states = arg_states(&args, 2, who)?;
            sheet.with(who, |p| {
                let c = resolve(p, who, &name, &variant, &states, None)?;
                to_value(&glaze_style::to_style(&c), who)
            })
        });
    }

    // glaze_at(name, variant, states, vw, vh) -> Style, resolved at a
    // given viewport size so `when vw < 600 { … }` breakpoints apply.
    // This is how a widget stays responsive to its pane size: pass the
    // `render(w, h)` arguments straight through.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_at", move |_vm, args| {
            let who = "glaze_at";
            let name = arg_str(&args, 0, who, "a style name")?;
            let variant = arg_variant(&args, 1, who)?;
            let states = arg_states(&args, 2, who)?;
            let vw = arg_f32(&args, 3, who, "vw")?;
            let vh = arg_f32(&args, 4, who, "vh")?;
            sheet.with(who, |p| {
                let c = resolve(p, who, &name, &variant, &states, Some((vw, vh)))?;
                to_value(&glaze_style::to_style(&c), who)
            })
        });
    }

    // glaze_slot(name, component[, variant[, states]]) -> the typed
    // per-slot style a component element expects (`tabs`, `table`, `bar`,
    // …). Glaze `part {}` blocks are how a stylesheet reaches inside a
    // compound component; this is the funct-side accessor for them.
    {
        let sheet = sheet.clone();
        vm.register_raw("glaze_slot", move |_vm, args| {
            let who = "glaze_slot";
            let name = arg_str(&args, 0, who, "a style name")?;
            let component = arg_str(&args, 1, who, "a component name")?;
            let variant = arg_variant(&args, 2, who)?;
            let states = arg_states(&args, 3, who)?;
            sheet.with(who, |p| slot_style(p, &name, &component, &variant, &states))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = r#"
        token surface = oklch(0.21 0.012 255)
        token gold    = oklch(0.80 0.13 85)
        token line    = oklch(0.45 0.012 255 / 0.5)
        fn space(n)   = n * 4px

        style card {
            fill   surface
            radius space(4)
            border line 1px
            pad    12px
        }

        style button(intent) {
            fill   intent == danger ? oklch(0.58 0.16 25) : gold
            radius 8px
            :focus { border gold 2px }
        }
    "#;

    fn sheet() -> GlazeSheet {
        let s = GlazeSheet::new();
        s.set(glaze::parse(SHEET).expect("test sheet must parse"), None);
        s
    }

    /// The whole point of the bridge: a resolved Glaze style must
    /// serialize into the exact record shape the renderer decodes back
    /// into a `protocol::Style`. If this round trip breaks, every styled
    /// widget silently loses its styling, so assert it end to end.
    #[test]
    fn resolved_style_round_trips_into_protocol_style() {
        let s = sheet();
        let value = s
            .with("glaze", |p| {
                let c = resolve(p, "glaze", "card", &HashMap::new(), &[], None)?;
                to_value(&glaze_style::to_style(&c), "glaze")
            })
            .expect("card must resolve");

        let json = value.to_json().expect("style must serialize to json");
        let style: crate::protocol::Style =
            serde_json::from_value(json).expect("must decode as protocol::Style");

        assert_eq!(style.radius.as_deref(), Some("16")); // space(4)
        let pad = style.padding.expect("card sets pad");
        assert_eq!(pad.top, 12.0);
        // fill + border, in paint order.
        assert_eq!(style.glaze_layers.len(), 2);
    }

    /// Variant params are compared as strings inside Glaze, so a static
    /// branch must actually fold differently per variant.
    #[test]
    fn variant_selects_a_different_plan() {
        let s = sheet();
        let fill_of = |intent: &str| {
            let mut v = HashMap::new();
            v.insert("intent".to_string(), intent.to_string());
            s.with("glaze", |p| {
                let c = resolve(p, "glaze", "button", &v, &[], None)?;
                Ok(match c.layers.first() {
                    Some(glaze::Layer::Fill(rgba)) => glaze_style::hex(*rgba),
                    other => panic!("expected a fill layer, got {other:?}"),
                })
            })
            .expect("button must resolve")
        };
        assert_ne!(fill_of("danger"), fill_of("primary"));
    }

    /// A discrete state adds its overlay plan on top of the base.
    #[test]
    fn state_overlay_adds_a_layer() {
        let s = sheet();
        let count = |states: &[String]| {
            s.with("glaze", |p| {
                let c = resolve(p, "glaze", "button", &HashMap::new(), states, None)?;
                Ok(c.layers.len())
            })
            .expect("button must resolve")
        };
        assert_eq!(count(&[]), 1);
        assert_eq!(count(&["focus".to_string()]), 2);
    }

    /// House rule: unknown style names are loud. A silent default here
    /// would render an unstyled element that looks like a layout bug.
    #[test]
    fn unknown_style_name_faults() {
        let s = sheet();
        let err = s
            .with("glaze", |p| {
                resolve(p, "glaze", "nope", &HashMap::new(), &[], None).map(|_| ())
            })
            .expect_err("unknown style must fault");
        assert!(
            format!("{err:?}").contains("nope"),
            "fault should name the missing style: {err:?}"
        );
    }

    /// Calling a resolver before loading a sheet must say so, rather than
    /// handing back an empty style.
    #[test]
    fn resolving_without_a_sheet_faults() {
        let s = GlazeSheet::new();
        let err = s.with("glaze", |_| Ok(())).expect_err("must fault");
        assert!(
            format!("{err:?}").contains("no stylesheet loaded"),
            "fault should explain what to call: {err:?}"
        );
    }

    /// Variant maps come from funct records, so scalars must survive the
    /// JSON hop as the strings Glaze compares against.
    #[test]
    fn variant_scalars_stringify() {
        let args = vec![
            Value::Unit,
            Value::from_json(&serde_json::json!({ "n": 2, "dark": true, "k": "v" })),
        ];
        let v = arg_variant(&args, 1, "glaze").expect("record must convert");
        assert_eq!(v.get("n").map(String::as_str), Some("2"));
        assert_eq!(v.get("dark").map(String::as_str), Some("true"));
        assert_eq!(v.get("k").map(String::as_str), Some("v"));
    }

    /// Typography can't come out of a style (styles are box-only), so the
    /// token accessor is what keeps font sizes and text colours in the
    /// sheet. Check both the numeric and colour paths, and that arithmetic
    /// / aliases fold the same way they do inside a style.
    #[test]
    fn tokens_resolve_to_numbers_and_colors() {
        let prog = glaze::parse(
            r#"
            token base = 16
            token h1   = base * 2
            token fg   = oklch(0.94 0.008 255)
            token ink  = fg
        "#,
        )
        .expect("sheet parses");

        assert_eq!(prog.token("h1").expect("h1"), glaze::Value::Num(32.0));
        let glaze::Value::Color(c) = prog.token("ink").expect("alias resolves") else {
            panic!("ink aliases a color");
        };
        assert_eq!(
            glaze_style::hex(c),
            glaze_style::hex(match prog.token("fg").expect("fg") {
                glaze::Value::Color(c) => c,
                other => panic!("fg is a color, got {other:?}"),
            })
        );
    }

    /// Same house rule as styles: a typo'd token name is loud.
    #[test]
    fn unknown_token_faults() {
        let prog = glaze::parse("token a = 1").expect("parses");
        let err = prog.token("b").expect_err("unknown token must error");
        assert!(
            format!("{err}").contains('b'),
            "error should name the token: {err}"
        );
    }

    /// A bare string is a convenient one-state shorthand.
    #[test]
    fn states_accept_a_bare_string() {
        let args = vec![Value::Unit, Value::Unit, Value::str("hover")];
        assert_eq!(
            arg_states(&args, 2, "glaze").expect("string state"),
            vec!["hover".to_string()]
        );
    }
}
