//! Pins style-muse's generated UI pack to the widget protocol's serde
//! shapes: every entry in `muse::ui_styles()` must deserialize into its
//! protocol type, or the Style Lab showcase would silently drop styling.

use inference_bevy::muse;
use widget_bevy::protocol::{
    BarStyle, CheckboxStyle, DialogStyle, PopoverStyle, RadioGroupStyle, SelectStyle, SliderStyle,
    StepperStyle, Style, TableStyle, TabsStyle, ToastStyle, ToggleStyle, TooltipStyle,
};

#[test]
fn ui_pack_matches_protocol_serde() {
    let mut rng = muse::Rng(0xC0FFEE);
    let model = muse::taste_model();
    for i in 0..40 {
        let g = muse::sample_genome(&mut rng, &model, &[]);
        let ui = muse::ui_styles(&g);
        let ctx = |k: &str| format!("genome #{i} ({}) key {k}", g.name);

        let _: Style = serde_json::from_value(ui["card"].clone()).expect("card");
        for key in ["button_primary", "button_secondary", "button_outline", "input", "badge"] {
            let s: Style = serde_json::from_value(ui[key].clone()).unwrap_or_else(|e| {
                panic!("{}: {e}\n{}", ctx(key), ui[key])
            });
            // candidate previews must never inherit active-theme label colors
            assert!(s.text_color.is_some(), "{} missing text_color", ctx(key));
        }
        // buttons must carry a hover plan so the renderer never falls back
        // to active-theme hover substitution on candidate cards
        for key in ["button_primary", "button_secondary", "button_outline"] {
            let s: Style = serde_json::from_value(ui[key].clone()).expect(key);
            assert!(s.hover.is_some(), "{} missing hover plan", ctx(key));
        }
        let _: ToggleStyle = serde_json::from_value(ui["toggle"].clone()).expect("toggle");
        let _: CheckboxStyle = serde_json::from_value(ui["checkbox"].clone()).expect("checkbox");
        let _: RadioGroupStyle = serde_json::from_value(ui["radio"].clone()).expect("radio");
        let _: SliderStyle = serde_json::from_value(ui["slider"].clone()).expect("slider");
        let _: StepperStyle = serde_json::from_value(ui["stepper"].clone()).expect("stepper");
        let _: SelectStyle = serde_json::from_value(ui["select"].clone()).expect("select");
        let _: TabsStyle = serde_json::from_value(ui["tabs"].clone()).expect("tabs");
        let _: TableStyle = serde_json::from_value(ui["table"].clone()).expect("table");
        let _: BarStyle = serde_json::from_value(ui["bar"].clone()).expect("bar");
        let _: ToastStyle = serde_json::from_value(ui["toast"].clone()).expect("toast");
        let _: PopoverStyle = serde_json::from_value(ui["popover"].clone()).expect("popover");
        let _: DialogStyle = serde_json::from_value(ui["dialog"].clone()).expect("dialog");
        let _: TooltipStyle = serde_json::from_value(ui["tooltip"].clone()).expect("tooltip");
    }
}
