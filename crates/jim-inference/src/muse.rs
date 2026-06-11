//! `style-muse` — generative theme engine behind the Style Lab widget.
//!
//! A theme is compressed into a small **genome** (hues, chroma, contrast,
//! shape, depth, motion, effect). The expander turns a genome into a
//! complete token set for `theme.ft` plus pane-chrome WGSL and a small
//! preview shader. Genomes come from three sources:
//!
//! - a taste-constrained random sampler,
//! - the same sampler biased by per-aspect like/dislike feedback
//!   (`~/.jim/style-lab/feedback.jsonl`),
//! - DeepSeek proposing genomes from the full feedback history
//!   (`llm::complete_json`).
//!
//! House rules baked in: focused-pane border/glow stays neutral (never
//! warm), all colors are OkLCh-derived and gamut-clamped, dark themes get
//! perceptually even surface ladders.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::io::Write as _;
use std::path::PathBuf;

use crate::llm::{self, LlmConfig};

// ---------------------------------------------------------------- rng

/// xorshift64* — deterministic, dependency-free. Seeded from the clock
/// by the bin; tests can seed explicitly.
pub struct Rng(pub u64);

impl Rng {
    pub fn from_clock() -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Rng(n | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.f32() * (hi - lo)
    }
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
    /// Sample an index proportionally to `weights` (all > 0).
    pub fn pick_weighted(&mut self, weights: &[f32]) -> usize {
        let total: f32 = weights.iter().sum();
        let mut t = self.f32() * total;
        for (i, w) in weights.iter().enumerate() {
            t -= w;
            if t <= 0.0 {
                return i;
            }
        }
        weights.len() - 1
    }
    /// Approximate normal via sum of uniforms (Irwin–Hall, n=4).
    fn gauss(&mut self, mean: f32, sd: f32) -> f32 {
        let s: f32 = (0..4).map(|_| self.f32()).sum::<f32>() - 2.0;
        mean + s * sd
    }
}

// ---------------------------------------------------------- color math

/// OkLCh → linear sRGB (Björn Ottosson's matrices). Returns components
/// possibly outside [0,1]; caller handles gamut.
fn oklch_to_linear_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let hr = h_deg.to_radians();
    let a = c * hr.cos();
    let b = c * hr.sin();
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3,
        -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3,
    ]
}

fn gamma_encode(u: f32) -> f32 {
    if u <= 0.003_130_8 {
        12.92 * u
    } else {
        1.055 * u.powf(1.0 / 2.4) - 0.055
    }
}

/// OkLCh → sRGB 0..1, reducing chroma until in gamut (hue/lightness
/// preserving clamp).
pub fn oklch_srgb(l: f32, c: f32, h: f32) -> [f32; 3] {
    let l = l.clamp(0.0, 1.0);
    let mut c = c.max(0.0);
    for _ in 0..32 {
        let rgb = oklch_to_linear_srgb(l, c, h);
        if rgb.iter().all(|v| (-0.0005..=1.0005).contains(v)) {
            return [
                gamma_encode(rgb[0].clamp(0.0, 1.0)),
                gamma_encode(rgb[1].clamp(0.0, 1.0)),
                gamma_encode(rgb[2].clamp(0.0, 1.0)),
            ];
        }
        c *= 0.92;
        if c < 0.0005 {
            c = 0.0;
        }
    }
    let rgb = oklch_to_linear_srgb(l, 0.0, h);
    [
        gamma_encode(rgb[0].clamp(0.0, 1.0)),
        gamma_encode(rgb[1].clamp(0.0, 1.0)),
        gamma_encode(rgb[2].clamp(0.0, 1.0)),
    ]
}

pub fn hex(l: f32, c: f32, h: f32) -> String {
    let [r, g, b] = oklch_srgb(l, c, h);
    format!(
        "#{:02x}{:02x}{:02x}",
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8
    )
}

pub fn hexa(l: f32, c: f32, h: f32, alpha: f32) -> String {
    let [r, g, b] = oklch_srgb(l, c, h);
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
        (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
    )
}

/// `vec3<f32>(r, g, b)` WGSL literal for baking theme colors into shaders.
fn wgsl_vec3(l: f32, c: f32, h: f32) -> String {
    let [r, g, b] = oklch_srgb(l, c, h);
    format!("vec3<f32>({:.4}, {:.4}, {:.4})", r, g, b)
}

// -------------------------------------------------------------- genome

pub const MODES: &[&str] = &["dark", "light"];
pub const HARMONIES: &[&str] = &["mono", "duotone", "complement"];
pub const CONTRASTS: &[&str] = &["soft", "medium", "high"];
/// `frame` is the only value that draws the two-tone outline (title strip
/// + margin ring); the rest are flat panes with at most a border line.
pub const BORDERS: &[&str] = &["none", "hairline", "strong", "frame"];
pub const DEPTHS: &[&str] = &["flat", "soft", "floaty", "glow"];
pub const GRADIENTS: &[&str] = &["none", "subtle", "bold"];
pub const MOTIONS: &[&str] = &["instant", "snappy", "smooth"];
pub const EASINGS: &[&str] = &["ease_out", "ease_in_out"];
pub const EFFECTS: &[&str] = &["none", "sheen", "aurora", "scanline", "grain", "ring_pulse"];
pub const HOVERS: &[&str] = &["tint", "glow", "outline", "lift"];
pub const RADII: &[f32] = &[0.0, 2.0, 4.0, 6.0, 10.0, 14.0];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genome {
    #[serde(default)]
    pub name: String,
    #[serde(default = "d_mode")]
    pub mode: String,
    /// Hue of the neutral surfaces, 0..360.
    #[serde(default = "d_hue")]
    pub base_hue: f32,
    /// Chroma of the neutral surfaces (0 = pure gray, ~0.035 = clearly tinted).
    #[serde(default)]
    pub surface_chroma: f32,
    #[serde(default = "d_hue")]
    pub accent_hue: f32,
    #[serde(default = "d_accent_chroma")]
    pub accent_chroma: f32,
    #[serde(default = "d_harmony")]
    pub harmony: String,
    #[serde(default = "d_contrast")]
    pub contrast: String,
    /// Pane corner radius in px; component radii derive from it.
    #[serde(default = "d_radius")]
    pub radius: f32,
    #[serde(default = "d_border")]
    pub border: String,
    #[serde(default = "d_depth")]
    pub depth: String,
    #[serde(default = "d_gradient")]
    pub gradient: String,
    #[serde(default = "d_motion")]
    pub motion: String,
    #[serde(default = "d_easing")]
    pub easing: String,
    /// Pane-chrome shader flavor.
    #[serde(default = "d_effect")]
    pub effect: String,
    #[serde(default = "d_strength")]
    pub effect_strength: f32,
    /// How interactive elements respond to the pointer.
    #[serde(default = "d_hover")]
    pub hover_style: String,
}

fn d_mode() -> String {
    "dark".into()
}
fn d_hue() -> f32 {
    250.0
}
fn d_accent_chroma() -> f32 {
    0.12
}
fn d_harmony() -> String {
    "duotone".into()
}
fn d_contrast() -> String {
    "medium".into()
}
fn d_radius() -> f32 {
    6.0
}
fn d_border() -> String {
    "hairline".into()
}
fn d_depth() -> String {
    "soft".into()
}
fn d_gradient() -> String {
    "none".into()
}
fn d_motion() -> String {
    "snappy".into()
}
fn d_easing() -> String {
    "ease_out".into()
}
fn d_effect() -> String {
    "none".into()
}
fn d_strength() -> f32 {
    0.2
}
fn d_hover() -> String {
    "tint".into()
}

impl Genome {
    /// Clamp ranges and replace invalid enum values (LLM output is
    /// validated here rather than trusted).
    pub fn sanitize(&mut self, rng: &mut Rng) {
        fn fix(v: &mut String, allowed: &[&str], rng: &mut Rng) {
            if !allowed.contains(&v.as_str()) {
                *v = (*rng.pick(allowed)).to_string();
            }
        }
        fix(&mut self.mode, MODES, rng);
        fix(&mut self.harmony, HARMONIES, rng);
        fix(&mut self.contrast, CONTRASTS, rng);
        fix(&mut self.border, BORDERS, rng);
        fix(&mut self.depth, DEPTHS, rng);
        fix(&mut self.gradient, GRADIENTS, rng);
        fix(&mut self.motion, MOTIONS, rng);
        fix(&mut self.easing, EASINGS, rng);
        fix(&mut self.effect, EFFECTS, rng);
        fix(&mut self.hover_style, HOVERS, rng);
        self.base_hue = self.base_hue.rem_euclid(360.0);
        self.accent_hue = self.accent_hue.rem_euclid(360.0);
        self.surface_chroma = self.surface_chroma.clamp(0.0, 0.04);
        self.accent_chroma = self.accent_chroma.clamp(0.04, 0.23);
        self.radius = self.radius.clamp(0.0, 16.0);
        self.effect_strength = self.effect_strength.clamp(0.05, 0.5);
        if self.name.trim().is_empty() {
            self.name = gen_name(rng);
        }
    }

    pub fn slug(&self) -> String {
        let mut s: String = self
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        while s.contains("--") {
            s = s.replace("--", "-");
        }
        s.trim_matches('-').to_string()
    }
}

const ADJECTIVES: &[&str] = &[
    "Quiet", "Velvet", "Copper", "Glacial", "Ember", "Mossy", "Night", "Saffron", "Cobalt",
    "Ashen", "Lacquer", "Hazel", "Tidal", "Iron", "Opal", "Cinder", "Juniper", "Misty", "Paper",
    "Tarnished", "Hollow", "Gilded", "Stray", "Deep",
];
const NOUNS: &[&str] = &[
    "Harbor", "Dusk", "Atelier", "Signal", "Garden", "Archive", "Tide", "Lantern", "Meridian",
    "Foundry", "Orchard", "Static", "Parlor", "Drift", "Veranda", "Reactor", "Library", "Canyon",
    "Studio", "Aerie", "Furnace", "Causeway", "Cellar", "Observatory",
];

fn gen_name(rng: &mut Rng) -> String {
    format!("{} {}", rng.pick(ADJECTIVES), rng.pick(NOUNS))
}

// ------------------------------------------------------------ feedback

/// One verdict from the Style Lab widget. `overall` is
/// `like | dislike | skip`; `aspects` values are `like | dislike`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEvent {
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub genes: Genome,
    #[serde(default)]
    pub overall: String,
    #[serde(default)]
    pub aspects: Map<String, Value>,
    #[serde(default)]
    pub note: String,
}

pub fn lab_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".jim").join("style-lab")
}

pub fn feedback_path() -> PathBuf {
    lab_dir().join("feedback.jsonl")
}

pub fn load_feedback() -> Vec<FeedbackEvent> {
    let Ok(text) = std::fs::read_to_string(feedback_path()) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn append_feedback(mut ev: FeedbackEvent) -> std::io::Result<()> {
    std::fs::create_dir_all(lab_dir())?;
    ev.ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(feedback_path())?;
    writeln!(f, "{}", serde_json::to_string(&ev).expect("serialize feedback"))?;
    Ok(())
}

/// Which feedback aspect governs which gene.
fn aspect_of(gene: &str) -> &'static str {
    match gene {
        "mode" | "harmony" | "contrast" => "colors",
        "radius" | "border" => "shape",
        "depth" | "gradient" => "depth",
        "motion" | "easing" => "motion",
        "hover_style" => "hover",
        "effect" => "effect",
        _ => "colors",
    }
}

/// (likes, dislikes) the event contributes to genes under `aspect`:
/// explicit aspect verdicts count 1.0, the overall verdict counts 0.5.
fn vote(ev: &FeedbackEvent, aspect: &str) -> (f32, f32) {
    let mut likes = 0.0;
    let mut dislikes = 0.0;
    match ev.aspects.get(aspect).and_then(|v| v.as_str()) {
        Some("like") => likes += 1.0,
        Some("dislike") => dislikes += 1.0,
        _ => {}
    }
    match ev.overall.as_str() {
        "like" => likes += 0.5,
        "dislike" => dislikes += 0.5,
        _ => {}
    }
    (likes, dislikes)
}

// ------------------------------------------------------------- sampler

struct TasteModel {
    events: Vec<FeedbackEvent>,
}

impl TasteModel {
    fn new(events: Vec<FeedbackEvent>) -> Self {
        TasteModel { events }
    }

    /// Laplace-smoothed preference score for a categorical gene value.
    fn score(&self, gene: &str, value: &str, get: impl Fn(&Genome) -> &str) -> f32 {
        let aspect = aspect_of(gene);
        let (mut l, mut d) = (0.0, 0.0);
        for ev in &self.events {
            if get(&ev.genes) == value {
                let (vl, vd) = vote(ev, aspect);
                l += vl;
                d += vd;
            }
        }
        (1.0 + l) / (2.0 + l + d)
    }

    fn pick_cat(
        &self,
        rng: &mut Rng,
        gene: &str,
        options: &[&str],
        prior: &[f32],
        get: impl Fn(&Genome) -> &str + Copy,
    ) -> String {
        let weights: Vec<f32> = options
            .iter()
            .zip(prior)
            .map(|(o, p)| p * self.score(gene, o, get))
            .collect();
        options[rng.pick_weighted(&weights)].to_string()
    }

    /// Hues from events whose `colors` aspect (or overall) was
    /// liked/disliked, for attraction/repulsion sampling.
    fn hue_votes(&self, get: impl Fn(&Genome) -> f32) -> (Vec<f32>, Vec<f32>) {
        let mut liked = Vec::new();
        let mut disliked = Vec::new();
        for ev in &self.events {
            let (l, d) = vote(ev, "colors");
            if l > d {
                liked.push(get(&ev.genes));
            } else if d > l {
                disliked.push(get(&ev.genes));
            }
        }
        (liked, disliked)
    }

    fn pick_hue(&self, rng: &mut Rng, get: impl Fn(&Genome) -> f32 + Copy, avoid: &[f32]) -> f32 {
        let (liked, disliked) = self.hue_votes(get);
        for _ in 0..10 {
            let h = if !liked.is_empty() && rng.f32() < 0.6 {
                let center = *rng.pick(&liked);
                rng.gauss(center, 18.0).rem_euclid(360.0)
            } else {
                rng.range(0.0, 360.0)
            };
            let near = |list: &[f32], deg: f32| {
                list.iter().any(|x| {
                    let mut diff = (x - h).abs() % 360.0;
                    if diff > 180.0 {
                        diff = 360.0 - diff;
                    }
                    diff < deg
                })
            };
            if near(&disliked, 16.0) {
                continue;
            }
            // batch diversity: keep accents spread out
            if near(avoid, 35.0) && rng.f32() < 0.8 {
                continue;
            }
            return h;
        }
        rng.range(0.0, 360.0)
    }
}

/// Sample one genome from priors × feedback weights. `avoid_hues` keeps
/// hues within a batch apart.
pub fn sample_genome(rng: &mut Rng, model: &TasteModelHandle, avoid_hues: &[f32]) -> Genome {
    let m = &model.0;
    let mode = m.pick_cat(rng, "mode", MODES, &[0.72, 0.28], |g| &g.mode);
    let accent_hue = m.pick_hue(rng, |g| g.accent_hue, avoid_hues);
    // Base (surface) hue: usually harmonizes with the accent — same hue
    // or shifted toward cool neutrals.
    let base_hue = match rng.pick_weighted(&[0.45, 0.35, 0.2]) {
        0 => accent_hue,
        1 => rng.gauss(250.0, 25.0).rem_euclid(360.0), // cool neutral
        _ => m.pick_hue(rng, |g| g.base_hue, &[]),
    };
    let harmony = m.pick_cat(rng, "harmony", HARMONIES, &[0.3, 0.45, 0.25], |g| &g.harmony);
    let contrast = m.pick_cat(rng, "contrast", CONTRASTS, &[0.3, 0.45, 0.25], |g| &g.contrast);
    let border = m.pick_cat(rng, "border", BORDERS, &[0.3, 0.35, 0.1, 0.25], |g| &g.border);
    let depth = m.pick_cat(rng, "depth", DEPTHS, &[0.2, 0.4, 0.25, 0.15], |g| &g.depth);
    let gradient = m.pick_cat(rng, "gradient", GRADIENTS, &[0.45, 0.4, 0.15], |g| &g.gradient);
    let motion = m.pick_cat(rng, "motion", MOTIONS, &[0.15, 0.5, 0.35], |g| &g.motion);
    let easing = m.pick_cat(rng, "easing", EASINGS, &[0.6, 0.4], |g| &g.easing);
    let effect = m.pick_cat(
        rng,
        "effect",
        EFFECTS,
        &[0.3, 0.2, 0.15, 0.1, 0.15, 0.1],
        |g| &g.effect,
    );
    let hover_style = m.pick_cat(rng, "hover_style", HOVERS, &[0.35, 0.25, 0.2, 0.2], |g| {
        &g.hover_style
    });
    // radius: categorical over the allowed steps, scored via shape aspect
    let radius_weights: Vec<f32> = RADII
        .iter()
        .zip([0.12, 0.15, 0.2, 0.25, 0.18, 0.1])
        .map(|(r, p)| {
            let aspect = "shape";
            let (mut l, mut d) = (0.0, 0.0);
            for ev in &m.events {
                if (ev.genes.radius - r).abs() < 1.5 {
                    let (vl, vd) = vote(ev, aspect);
                    l += vl;
                    d += vd;
                }
            }
            p * (1.0 + l) / (2.0 + l + d)
        })
        .collect();
    let radius = RADII[rng.pick_weighted(&radius_weights)];

    let mut g = Genome {
        name: gen_name(rng),
        mode,
        base_hue,
        surface_chroma: rng.range(0.0, 0.035),
        accent_hue,
        accent_chroma: rng.range(0.07, 0.2),
        harmony,
        contrast,
        radius,
        border,
        depth,
        gradient,
        motion,
        easing,
        effect,
        effect_strength: rng.range(0.1, 0.35),
        hover_style,
    };
    g.sanitize(rng);
    g
}

/// Opaque handle so the bin doesn't see TasteModel internals.
pub struct TasteModelHandle(TasteModel);

pub fn taste_model() -> TasteModelHandle {
    TasteModelHandle(TasteModel::new(load_feedback()))
}

// --------------------------------------------------------------- LLM

#[derive(Deserialize)]
struct MuseBatch {
    candidates: Vec<Genome>,
}

const MUSE_SYSTEM: &str = r#"You are a senior UI designer generating color/style themes for a code editor with floating panes (terminals, text editors, widgets) on a 2D canvas. You output theme GENOMES as JSON.

A genome has exactly these fields:
- name: string — short evocative two-word name ("Copper Dusk")
- mode: "dark" | "light"
- base_hue: number 0-360 — OkLCh hue of the neutral surfaces
- surface_chroma: number 0-0.04 — how tinted the neutrals are (0 = gray)
- accent_hue: number 0-360 — OkLCh hue of the accent color
- accent_chroma: number 0.04-0.23 — accent vividness
- harmony: "mono" | "duotone" | "complement" — how syntax-highlight hues derive from the accent
- contrast: "soft" | "medium" | "high" — text-vs-background contrast
- radius: number, one of 0, 2, 4, 6, 10, 14 — pane corner radius px
- border: "none" | "hairline" | "strong" | "frame" — pane outline treatment; only "frame" draws a two-tone frame (title strip + margin ring), the others are flat panes
- depth: "flat" | "soft" | "floaty" | "glow" — shadow treatment
- gradient: "none" | "subtle" | "bold" — gradient on primary buttons
- motion: "instant" | "snappy" | "smooth" — UI transition speed
- easing: "ease_out" | "ease_in_out"
- effect: "none" | "sheen" | "aurora" | "scanline" | "grain" | "ring_pulse" — animated pane-chrome shader flavor
- effect_strength: number 0.05-0.5
- hover_style: "tint" | "glow" | "outline" | "lift" — how buttons react to the pointer (background shift / soft animated glow / accent outline / raised shadow)

Design principles:
- Aim for cohesive, restrained, professional looks with one memorable idea each.
- Vary the batch widely: different hue families, at least one light theme sometimes, different shape/depth personalities.
- Respect the user's feedback: stay close to genes they liked, avoid genes they disliked, and treat free-text notes as direct instructions.
- Strong effects (scanline, aurora) pair best with low surface_chroma and hairline borders.

Respond ONLY with JSON: {"candidates": [genome, ...]} with exactly the requested count."#;

pub fn llm_genomes(count: usize, note: &str, rng: &mut Rng) -> Result<Vec<Genome>, String> {
    let cfg = LlmConfig::from_env().map_err(|e| e.to_string())?;
    let events = load_feedback();
    let recent: Vec<&FeedbackEvent> = events.iter().rev().take(40).collect();
    let history: Vec<Value> = recent
        .iter()
        .rev()
        .map(|ev| {
            json!({
                "name": ev.name,
                "genes": ev.genes,
                "overall": ev.overall,
                "aspects": ev.aspects,
                "note": ev.note,
            })
        })
        .collect();
    let user = serde_json::to_string_pretty(&json!({
        "request": format!("Propose {} new theme genomes.", count),
        "feedback_history": history,
        "user_note": note,
    }))
    .expect("serialize muse request");
    let batch: MuseBatch =
        llm::complete_json(&cfg, MUSE_SYSTEM, &user, 1.1).map_err(|e| e.to_string())?;
    let mut out = batch.candidates;
    out.truncate(count);
    for g in &mut out {
        g.sanitize(rng);
    }
    if out.is_empty() {
        return Err("model returned no candidates".into());
    }
    Ok(out)
}

// ------------------------------------------------------------ expander

struct Ramp {
    /// background of the infinite canvas behind panes
    canvas: f32,
    /// core editor/terminal background
    bg: f32,
    /// pane chrome body
    pane: f32,
    s1: f32,
    s2: f32,
    s3: f32,
    input: f32,
    fg: f32,
    muted: f32,
    dark: bool,
}

fn ramp(g: &Genome) -> Ramp {
    let dark = g.mode != "light";
    if dark {
        let fg = match g.contrast.as_str() {
            "soft" => 0.82,
            "high" => 0.94,
            _ => 0.88,
        };
        Ramp {
            canvas: 0.115,
            bg: 0.145,
            pane: 0.185,
            s1: 0.215,
            s2: 0.245,
            s3: 0.28,
            input: 0.125,
            fg,
            muted: 0.58,
            dark,
        }
    } else {
        let fg = match g.contrast.as_str() {
            "soft" => 0.34,
            "high" => 0.17,
            _ => 0.26,
        };
        Ramp {
            canvas: 0.93,
            bg: 0.965,
            pane: 0.982,
            s1: 0.94,
            s2: 0.915,
            s3: 0.885,
            input: 0.995,
            fg,
            muted: 0.5,
            dark,
        }
    }
}

/// Hue used by "secondary" roles (strings, links) per harmony.
fn secondary_hue(g: &Genome) -> f32 {
    match g.harmony.as_str() {
        "mono" => g.accent_hue,
        "complement" => (g.accent_hue + 180.0).rem_euclid(360.0),
        _ => (g.accent_hue + 65.0).rem_euclid(360.0),
    }
}

/// Expand a genome into the full theme token map, in display order.
pub fn expand_tokens(g: &Genome) -> Vec<(String, Value)> {
    let r = ramp(g);
    let dark = r.dark;
    let bh = g.base_hue;
    let sc = g.surface_chroma;
    let ah = g.accent_hue;
    let sh = secondary_hue(g);
    let ac = g.accent_chroma;
    // accent lightness tuned for mode so it reads on surfaces
    let al = if dark { 0.74 } else { 0.55 };
    let accent = hex(al, ac, ah);
    // syntax chroma: readable but lively
    let syc = (ac * 0.85).clamp(0.06, 0.14);
    let syl = if dark { 0.78 } else { 0.45 };
    let syl2 = if dark { 0.72 } else { 0.52 };
    // semantic hues are fixed; lightness/chroma match the theme
    let (sem_l, sem_c) = if dark { (0.74, 0.13) } else { (0.55, 0.14) };

    let border_w: f64 = match g.border.as_str() {
        "none" => 0.0,
        "strong" => 2.0,
        _ => 1.0,
    };
    let edge = if dark { r.pane + 0.10 } else { r.pane - 0.13 };
    let edge_strong = if dark { r.pane + 0.24 } else { r.pane - 0.32 };
    let border_col = match g.border.as_str() {
        "none" => hex(r.pane, sc, bh), // invisible against the pane
        "strong" => hex(edge_strong, sc * 1.2, bh),
        _ => hex(edge, sc, bh),
    };
    // Only the `frame` treatment draws the two-tone outline (title strip
    // + margin ring); everything else is a flat pane — the ring color
    // matches the body so no outline exists, focused or not.
    let framed = g.border == "frame";

    // Focus is QUIET: the title strip + margin ring lighten one step
    // (chrome_title_bg_focused below); the border NEVER changes and
    // there is no glow — a bright focus outline reads as the whole
    // pane changing and is exactly what we're avoiding.
    let focus_border = border_col.clone();
    let focus_glow = hex(if dark { 0.7 } else { 0.55 }, 0.025, 248.0);
    let (focus_w, focus_strength): (f64, f64) = (0.0, 0.0);

    // shadows
    let sh_l = if dark { 0.02 } else { 0.25 };
    let (shadow_color, shadow_blur, shadow_dy): (String, f64, f64) = match g.depth.as_str() {
        "flat" => (hexa(sh_l, sc, bh, 0.0), 1.0, 0.0),
        "soft" => (hexa(sh_l, sc, bh, if dark { 0.42 } else { 0.18 }), 20.0, 5.0),
        "floaty" => (hexa(sh_l, sc, bh, if dark { 0.55 } else { 0.25 }), 34.0, 11.0),
        // glow: accent-tinted ambient halo, no offset
        _ => (hexa(al, ac * 0.55, ah, if dark { 0.30 } else { 0.20 }), 26.0, 0.0),
    };

    let primary_label = if dark {
        hex(0.13, (ac * 0.4).min(0.04), ah)
    } else {
        hex(0.985, 0.005, ah)
    };
    let btn_radius = (g.radius - 2.0).max(0.0) as f64;
    let sel_alpha = 0.32;

    let mut t: Vec<(String, Value)> = Vec::with_capacity(96);
    macro_rules! st {
        ($k:expr, $v:expr) => {
            t.push(($k.to_string(), Value::String($v)))
        };
    }
    macro_rules! nm {
        ($k:expr, $v:expr) => {
            t.push((
                $k.to_string(),
                Value::Number(serde_json::Number::from_f64($v as f64).expect("finite")),
            ))
        };
    }
    st!("pane_bg", hex(r.pane, sc, bh));
    st!("pane_border", border_col.clone());
    st!("pane_border_focused", focus_border.clone());
    st!("pane_focus_glow", focus_glow);
    nm!("pane_corner_radius", g.radius);
    nm!("pane_border_width", border_w.max(0.0));
    nm!("pane_border_width_focused", border_w.max(0.0));
    nm!("pane_focus_width", focus_w);
    nm!("pane_focus_strength", focus_strength);
    st!("pane_shadow_color", shadow_color.clone());
    nm!("pane_shadow_blur", shadow_blur);
    nm!("pane_shadow_offset_y", shadow_dy);

    st!("bg", hex(r.bg, sc, bh));
    st!("fg", hex(r.fg, (sc * 0.6).min(0.015), bh));
    st!("fg_muted", hex(r.muted, (sc * 0.8).min(0.02), bh));
    st!("accent", accent.clone());
    st!("caret", accent.clone());
    st!("selection", hexa(al, ac * 0.6, ah, sel_alpha * 0.6));
    st!("warn", hex(sem_l, sem_c, 85.0));
    st!("err", hex(sem_l, sem_c + 0.02, 25.0));

    st!("chrome_title", hex(r.muted, sc, bh));
    st!("chrome_title_focused", hex(r.fg, sc * 0.5, bh));
    // chrome_title_bg paints the title strip + margin ring around the
    // content. Its focused variant lightening ONE QUIET STEP is the
    // app's whole focus indicator. Framed themes have a distinct ring
    // at rest; flat themes show no frame until focused.
    let step = if dark { 0.035 } else { -0.03 };
    if framed {
        let ring_l = if dark { r.pane - 0.04 } else { r.pane + 0.012 };
        st!("chrome_title_bg", hex(ring_l, sc, bh));
        st!("chrome_title_bg_focused", hex(ring_l + step * 1.4, (sc * 1.4).min(0.045), bh));
    } else {
        st!("chrome_title_bg", hex(r.pane, sc, bh));
        st!("chrome_title_bg_focused", hex(r.pane + step, sc, bh));
    }
    st!(
        "chrome_divider",
        match g.border.as_str() {
            "frame" => hex(if dark { r.pane + 0.06 } else { r.pane - 0.08 }, sc, bh),
            "none" => hex(r.pane, sc, bh), // invisible
            _ => hex(if dark { r.pane + 0.03 } else { r.pane - 0.04 }, sc, bh),
        }
    );
    st!("chrome_close", hex(r.muted, sc, bh));
    st!("chrome_handle", hex(if dark { r.pane + 0.12 } else { r.pane - 0.16 }, sc, bh));

    st!("syntax_default", hex(r.fg, (sc * 0.6).min(0.015), bh));
    st!("syntax_keyword", hex(syl, syc, ah));
    st!("syntax_string", hex(syl2, syc, sh));
    st!("syntax_comment", hex(if dark { 0.5 } else { 0.62 }, (sc * 1.2).min(0.03), bh));
    st!("syntax_function", hex(syl + if dark { 0.04 } else { -0.04 }, syc, (ah + 20.0).rem_euclid(360.0)));
    st!("syntax_type", hex(syl2, syc * 0.9, (sh + 30.0).rem_euclid(360.0)));
    st!("syntax_attribute", hex(syl2, syc * 0.8, (ah - 35.0).rem_euclid(360.0)));
    st!("syntax_constant", hex(syl2, syc, (ah - 60.0).rem_euclid(360.0)));
    st!("syntax_operator", hex(if dark { 0.66 } else { 0.42 }, syc * 0.35, bh));
    st!("syntax_punctuation", hex(r.muted, syc * 0.25, bh));
    st!("syntax_variable", hex(r.fg, (sc * 0.6).min(0.015), bh));
    st!("syntax_property", hex(syl2, syc * 0.7, (ah + 45.0).rem_euclid(360.0)));
    st!("syntax_label", hex(syl2, syc, (ah - 60.0).rem_euclid(360.0)));
    st!("syntax_escape", hex(sem_l, sem_c * 0.9, 85.0));
    st!("syntax_constructor", hex(syl2, syc * 0.9, (sh + 30.0).rem_euclid(360.0)));

    st!("input_bg", hex(r.input, sc, bh));
    st!("input_text", hex(r.fg - if dark { 0.06 } else { -0.06 }, sc * 0.6, bh));
    st!("input_text_focused", hex(r.fg + if dark { 0.04 } else { -0.04 }, sc * 0.5, bh));

    st!("button_bg", hex(r.s2, sc, bh));
    // hover token tuned by the hover gene: glow tints toward the accent,
    // outline barely moves, tint/lift shift the surface clearly
    let hover_bg = match g.hover_style.as_str() {
        "glow" => hex(
            if dark { r.s3 + 0.02 } else { r.s3 - 0.02 },
            (sc + ac * 0.25).min(0.06),
            ah,
        ),
        "outline" => hex(if dark { r.s2 + 0.02 } else { r.s2 - 0.02 }, sc, bh),
        _ => hex(if dark { r.s3 + 0.04 } else { r.s3 - 0.04 }, sc, bh),
    };
    st!("button_bg_hover", hover_bg);
    st!("button_label", hex(r.fg, sc * 0.5, bh));
    st!("button_primary_bg", accent.clone());
    st!("button_primary_label", primary_label.clone());

    nm!("widget_button_corner_radius", btn_radius);
    st!("widget_button_border", border_col.clone());
    nm!("widget_button_border_width", if g.border == "none" { 0.0 } else { 1.0 });
    st!(
        "widget_button_shadow_color",
        if g.depth == "flat" { hexa(sh_l, sc, bh, 0.0) } else { hexa(sh_l, sc, bh, if dark { 0.5 } else { 0.18 }) }
    );
    nm!("widget_button_shadow_blur", if g.depth == "flat" { 0.0 } else { 8.0 });
    nm!("widget_button_shadow_offset_y", if g.depth == "flat" { 0.0 } else { 3.0 });

    st!("status_idle", accent.clone());
    st!("status_running", hex(sem_l, sem_c, 85.0));
    st!("status_success", hex(sem_l, sem_c, 150.0));
    st!("status_failed", hex(sem_l, sem_c + 0.02, 25.0));

    st!("radial_wedge", hex(r.s2, sc, bh));
    st!("radial_wedge_hover", accent.clone());
    st!("radial_deadzone", hex(r.bg, sc, bh));
    st!("radial_deadzone_ring", hex(r.muted, sc, bh));
    st!("radial_label", hex(r.fg, sc * 0.5, bh));
    st!("radial_label_hover", primary_label.clone());
    st!("radial_icon", hex(r.fg, sc * 0.5, bh));
    st!("radial_backdrop", hexa(r.bg, sc, bh, 0.25));

    st!("widget_bar_track", hex(r.s2, sc, bh));
    st!("widget_bar_fill", accent.clone());
    st!("widget_badge_bg", accent.clone());
    st!("widget_badge_label", primary_label);
    st!("widget_link", hex(if dark { 0.74 } else { 0.5 }, syc, sh));

    st!("canvas_bg", hex(r.canvas, sc, bh));
    st!("sidebar_bg", hex(if dark { r.canvas + 0.015 } else { r.canvas + 0.015 }, sc, bh));
    st!("sidebar_row_active_bg", hex(if dark { r.canvas + 0.06 } else { r.canvas - 0.05 }, sc, bh));
    st!("sidebar_row_renaming_bg", hex(if dark { r.canvas + 0.04 } else { r.canvas - 0.03 }, sc, bh));
    st!("sidebar_text_faint", hex(if dark { 0.45 } else { 0.68 }, sc, bh));

    st!("surface_1", hex(r.s1, sc, bh));
    st!("surface_2", hex(r.s2, sc, bh));
    st!("surface_3", hex(r.s3, sc, bh));

    // accent ramp 50..900 (light → dark)
    let ramp_ls: [f32; 10] = [0.95, 0.9, 0.83, 0.76, 0.7, 0.62, 0.54, 0.45, 0.36, 0.27];
    for (i, name) in ["accent_50", "accent_100", "accent_200", "accent_300", "accent_400", "accent_500", "accent_600", "accent_700", "accent_800", "accent_900"]
        .iter()
        .enumerate()
    {
        let cl = ramp_ls[i];
        let cc = ac * (1.0 - (cl - 0.62).abs() * 0.9);
        st!(*name, hex(cl, cc.max(0.02), ah));
    }

    nm!("radius_xs", (g.radius * 0.35).round().max(0.0));
    nm!("radius_sm", (g.radius * 0.65).round());
    nm!("radius_md", g.radius);
    nm!("radius_lg", (g.radius * 1.6).round());
    nm!("radius_pill", 999.0);

    st!("shadow_sm_color", if g.depth == "flat" { hexa(sh_l, sc, bh, 0.0) } else { hexa(sh_l, sc, bh, if dark { 0.35 } else { 0.12 }) });
    nm!("shadow_sm_blur", 6.0);
    nm!("shadow_sm_offset_y", 2.0);
    st!("shadow_md_color", shadow_color.clone());
    nm!("shadow_md_blur", 14.0);
    nm!("shadow_md_offset_y", 4.0);
    st!("shadow_lg_color", shadow_color);
    nm!("shadow_lg_blur", 30.0);
    nm!("shadow_lg_offset_y", 10.0);

    t
}

// -------------------------------------------------------- chrome WGSL

const CHROME_SCAFFOLD: &str = r#"#import bevy_sprite::mesh2d_vertex_output::VertexOutput

struct ChromeParams {
    size: vec2<f32>,
    corner_radius: f32,
    border_width: f32,
    bg: vec4<f32>,
    border: vec4<f32>,
    focus: vec4<f32>,
    focus_width: f32,
    time: f32,
    cover_mode: f32,
    title_h: f32,
    title_bg: vec4<f32>,
    content_margin: f32,
    _pad_r0: f32,
    _pad_r1: f32,
    _pad_r2: f32,
}

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> params: ChromeParams;

fn rounded_rect_sdf(p: vec2<f32>, half_size: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half_size + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn hash21(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let p = (in.uv - vec2<f32>(0.5)) * params.size;
    let half_size = params.size * 0.5;
    let r = min(params.corner_radius, min(half_size.x, half_size.y));
    let d = rounded_rect_sdf(p, half_size, r);

    var coverage = 1.0 - smoothstep(-0.5, 0.5, d);
    if (coverage <= 0.0) {
        return vec4<f32>(0.0);
    }

    // title-cover quad: paint only the title strip (outline color)
    if (params.cover_mode > 0.5) {
        let y_from_top = in.uv.y * params.size.y;
        let cover_mask = 1.0 - smoothstep(params.title_h - 0.5, params.title_h + 0.5, y_from_top);
        if (cover_mask <= 0.0) {
            return vec4<f32>(0.0);
        }
        return vec4<f32>(params.title_bg.rgb, coverage * cover_mask * params.title_bg.a);
    }

    // two-tone body: outline (title strip + margin ring) in title_bg,
    // content backdrop in bg — focus re-colors the outline only
    var color = params.bg.rgb;
    if (params.content_margin > 0.0 && params.title_bg.a > 0.0) {
        let px = in.uv * params.size;
        let m = params.content_margin;
        let c_min = vec2<f32>(m, params.title_h + m);
        let c_max = params.size - vec2<f32>(m, m);
        let cd = max(
            max(c_min.x - px.x, px.x - c_max.x),
            max(c_min.y - px.y, px.y - c_max.y),
        );
        color = mix(params.bg.rgb, params.title_bg.rgb, smoothstep(-0.5, 0.5, cd));
    }
    // gentle top light so the body never reads as a flat slab
    color = color * (1.0 + 0.05 * (0.5 - in.uv.y));

//EFFECT//

    let bw = max(params.border_width, 0.0);
    if (bw > 0.0) {
        // band hugs the edge: ~0 deep inside, 1 within border_width
        let band = smoothstep(-bw - 0.6, -bw + 0.6, d);
        color = mix(color, params.border.rgb, band * params.border.a);
    }

    // inner focus glow — color comes from the theme (kept neutral)
    if (params.focus_width > 0.001 && params.focus.a > 0.001) {
        let inset = clamp((-d - bw) / params.focus_width, 0.0, 1.0);
        let glow = 1.0 - inset;
        color = color + params.focus.rgb * glow * glow * params.focus.a;
    }

    return vec4<f32>(color, coverage * params.bg.a);
}
"#;

/// Pane-chrome shader for the genome's effect, with theme colors baked
/// in as literals. `none` yields a static scaffold (no `params.time`,
/// so the app stays in reactive render mode).
pub fn chrome_wgsl(g: &Genome) -> String {
    let s = g.effect_strength;
    let dark = g.mode != "light";
    let al = if dark { 0.74 } else { 0.55 };
    let accent = wgsl_vec3(al, g.accent_chroma, g.accent_hue);
    let second = wgsl_vec3(al, g.accent_chroma * 0.9, secondary_hue(g));
    let body = match g.effect.as_str() {
        "sheen" => format!(
            "    // static diagonal sheen (x*x, not pow: pow(neg, 2) is NaN in WGSL)\n\
             \x20   let sx = (in.uv.x + in.uv.y * 0.35 - 0.42) * 3.8;\n\
             \x20   let sweep = exp(-sx * sx);\n\
             \x20   color = color + vec3<f32>(1.0) * sweep * {s:.3} * 0.12;\n"
        ),
        "aurora" => format!(
            "    // slow aurora wash along the top edge\n\
             \x20   let t = params.time * 0.11;\n\
             \x20   let x1 = (in.uv.x - (0.30 + 0.24 * sin(t * 2.1))) * 2.6;\n\
             \x20   let x2 = (in.uv.x - (0.68 + 0.20 * sin(t * 1.4 + 2.1))) * 2.3;\n\
             \x20   let g1 = exp(-x1 * x1);\n\
             \x20   let g2 = exp(-x2 * x2);\n\
             \x20   let band = exp(-in.uv.y * 5.0);\n\
             \x20   color = color + ({accent} * g1 + {second} * g2) * band * {s:.3};\n"
        ),
        "scanline" => format!(
            "    // phosphor scanlines + faint flicker\n\
             \x20   let sl = 0.5 + 0.5 * sin(p.y * 3.14159);\n\
             \x20   let flicker = 1.0 + 0.012 * sin(params.time * 9.0);\n\
             \x20   color = color * (1.0 - {s:.3} * 0.45 * sl) * flicker;\n\
             \x20   color = color + {accent} * exp(-abs(d) * 0.18) * {s:.3} * 0.25;\n"
        ),
        "grain" => format!(
            "    // static paper grain\n\
             \x20   let n = hash21(floor(p * 1.4));\n\
             \x20   color = color * (1.0 + (n - 0.5) * {s:.3} * 0.5);\n"
        ),
        "ring_pulse" => format!(
            "    // breathing accent rim just inside the border\n\
             \x20   let pulse = 0.5 + 0.5 * sin(params.time * 1.7);\n\
             \x20   let rim = smoothstep(-params.border_width - 3.0, -params.border_width, d);\n\
             \x20   color = color + {accent} * rim * pulse * {s:.3} * 0.8;\n"
        ),
        _ => String::new(),
    };
    CHROME_SCAFFOLD.replace("//EFFECT//", &body)
}

/// Small animated WGSL *glaze body* for the widget's per-card effect
/// strip (`u.*` uniforms, `in.uv`; premultiplied output).
pub fn preview_wgsl(g: &Genome) -> String {
    let s = g.effect_strength;
    let dark = g.mode != "light";
    let al = if dark { 0.78 } else { 0.55 };
    let accent = wgsl_vec3(al, g.accent_chroma, g.accent_hue);
    let second = wgsl_vec3(al, g.accent_chroma * 0.9, secondary_hue(g));
    match g.effect.as_str() {
        "sheen" => format!(
            "let x = fract(u.time * 0.22);\n\
             let dx = (in.uv.x - x) * 7.0;\n\
             let sw = exp(-dx * dx);\n\
             let a = sw * {s:.3} * 1.6;\n\
             return vec4<f32>(vec3<f32>(1.0) * a, a);"
        ),
        "aurora" => format!(
            "let t = u.time * 0.3;\n\
             let x1 = (in.uv.x - (0.3 + 0.25 * sin(t * 1.9))) * 2.6;\n\
             let x2 = (in.uv.x - (0.7 + 0.22 * sin(t * 1.2 + 2.0))) * 2.2;\n\
             let g1 = exp(-x1 * x1);\n\
             let g2 = exp(-x2 * x2);\n\
             let band = 0.4 + 0.6 * exp(-in.uv.y * 2.0);\n\
             let col = ({accent} * g1 + {second} * g2) * band * {s:.3} * 2.2;\n\
             let a = max(max(col.r, col.g), col.b) * 0.8;\n\
             return vec4<f32>(col, a);"
        ),
        "scanline" => format!(
            "let sl = 0.5 + 0.5 * sin(in.uv.y * u.size.y * 3.14159);\n\
             let dy = (in.uv.y - fract(u.time * 0.21)) * 7.0;\n\
             let scroll = exp(-dy * dy);\n\
             let dim = sl * {s:.3} * 0.55;\n\
             let glow = {accent} * scroll * {s:.3} * 1.2;\n\
             return vec4<f32>(glow, clamp(dim + scroll * {s:.3} * 0.4, 0.0, 1.0));"
        ),
        "grain" => format!(
            "let cell = floor(in.uv * u.size * 0.7) + floor(u.time * 6.0) * 13.7;\n\
             var p3 = fract(vec3<f32>(cell.x, cell.y, cell.x) * 0.1031);\n\
             p3 = p3 + dot(p3, p3.yzx + 33.33);\n\
             let n = fract((p3.x + p3.y) * p3.z);\n\
             let a = step(0.93, n) * {s:.3} * 1.8;\n\
             return vec4<f32>({accent} * a, a * 0.8);"
        ),
        "ring_pulse" => format!(
            "let pp = (in.uv - vec2<f32>(0.5)) * u.size;\n\
             let pulse = 0.5 + 0.5 * sin(u.time * 2.2);\n\
             let rad = mix(6.0, min(u.size.x, u.size.y) * 0.42, pulse);\n\
             let ring = exp(-abs(length(pp) - rad) * 0.35);\n\
             let a = ring * {s:.3} * 2.0;\n\
             return vec4<f32>({accent} * a, a);"
        ),
        _ => String::new(),
    }
}

// ------------------------------------------------------------ ui pack

/// White-veil shader for `hover_style: glow` — `u.hover` is eased
/// per-element by the host, so the bloom animates with zero CPU work.
fn hover_glow_wgsl() -> String {
    "let a = smoothstep(0.0, 1.0, u.hover) * 0.22 * (1.35 - in.uv.y * 0.7);\n\
     return vec4<f32>(vec3<f32>(1.0) * a, a);"
        .to_string()
}

/// Complete inline styles for every widget element, painted in the
/// genome's own colors. The Style Lab showcase consumes this verbatim,
/// so candidates render identically regardless of the active theme.
pub fn ui_styles(g: &Genome) -> Value {
    let r = ramp(g);
    let dark = r.dark;
    let (bh, sc, ah, ac) = (g.base_hue, g.surface_chroma, g.accent_hue, g.accent_chroma);
    let al = if dark { 0.74 } else { 0.55 };
    // hover moves toward light in dark themes, toward ink in light ones
    let lift = if dark { 1.0_f32 } else { -1.0 };

    let pane = hex(r.pane, sc, bh);
    let s1 = hex(r.s1, sc, bh);
    let s2 = hex(r.s2, sc, bh);
    let s3 = hex(r.s3, sc, bh);
    let s_hot = hex(r.s3 + 0.045 * lift, sc, bh);
    let input = hex(r.input, sc, bh);
    let fg = hex(r.fg, (sc * 0.6).min(0.015), bh);
    let muted = hex(r.muted, (sc * 0.8).min(0.02), bh);
    let accent = hex(al, ac, ah);
    let accent_hot = hex(al + 0.06 * lift, ac, ah);
    let accent_soft = hexa(al, ac, ah, 0.22);
    let primary_label = if dark {
        hex(0.13, (ac * 0.4).min(0.04), ah)
    } else {
        hex(0.985, 0.005, ah)
    };
    let edge = hex(if dark { r.pane + 0.10 } else { r.pane - 0.13 }, sc, bh);
    let sh_l = if dark { 0.02 } else { 0.25 };
    let shadow_col = hexa(sh_l, sc, bh, if dark { 0.45 } else { 0.2 });

    let btn_r = format!("{}", (g.radius - 2.0).max(0.0));
    let r_sm = format!("{}", (g.radius * 0.65).round().max(1.0));
    let r_md = format!("{}", g.radius.max(2.0));
    let r_lg = format!("{}", (g.radius * 1.6).round().max(4.0));
    let pill = "999";

    let tms = transition_ms(g).min(180.0);
    let transitions = if tms > 0.0 {
        json!([{"state": "hover", "duration_ms": tms, "easing": g.easing}])
    } else {
        json!([])
    };
    let btn_pad = json!({"top": 6.0, "right": 14.0, "bottom": 6.0, "left": 14.0});

    // hover overlay for a flat-background button. Always present so the
    // renderer's theme-token hover substitution never fires on candidate
    // previews (the active theme's colors would bleed in).
    let flat_hover = |base_hot: &str| -> Value {
        match g.hover_style.as_str() {
            "outline" => json!({"border": {"color": accent, "width": 1.5}}),
            "lift" => json!({
                "background": base_hot,
                "shadow": {"color": shadow_col, "blur": 16.0, "offset_y": 6.0},
            }),
            _ => json!({"background": base_hot}),
        }
    };

    // ---- primary button: gradient / glow get glaze layer plans
    let mut primary = json!({
        "radius": btn_r, "padding": btn_pad, "transitions": transitions,
        "text_color": primary_label,
    });
    let grad = gradient_stops(g);
    let glow = g.hover_style == "glow";
    if grad.is_some() || glow {
        let mut layers = vec![match &grad {
            Some([c1, c2]) => json!({"type": "linear-gradient", "angle": 90.0,
                "stops": [{"offset": 0.0, "color": c2}, {"offset": 1.0, "color": c1}]}),
            None => json!({"type": "fill", "color": accent}),
        }];
        if glow {
            layers.push(json!({"type": "shader", "body": hover_glow_wgsl(), "overlay": true}));
        }
        primary["glaze_layers"] = json!(layers);
        if glow {
            // shader already animates via u.hover; overlay only suppresses
            // the built-in substitution
            primary["hover"] = json!({"background": accent});
        } else {
            // brighten the gradient on hover — same layer shape, so the
            // renderer lerps it smoothly
            let [h1, h2] = match &grad {
                Some(_) => [
                    hex(al + 0.1 * lift, ac, (ah - 18.0).rem_euclid(360.0)),
                    hex(al + 0.02 * lift, ac * 1.1, (ah + 22.0).rem_euclid(360.0)),
                ],
                None => [accent_hot.clone(), accent_hot.clone()],
            };
            primary["hover"] = json!({"glaze_layers": [{"type": "linear-gradient", "angle": 90.0,
                "stops": [{"offset": 0.0, "color": h2}, {"offset": 1.0, "color": h1}]}]});
        }
    } else {
        primary["background"] = json!(accent);
        primary["hover"] = flat_hover(&accent_hot);
    }

    let secondary = json!({
        "background": s2, "radius": btn_r, "padding": btn_pad,
        "border": {"color": edge, "width": 1.0}, "text_color": fg,
        "transitions": transitions, "hover": flat_hover(&s_hot),
    });
    let outline_hover = match g.hover_style.as_str() {
        "outline" | "glow" => json!({"background": accent_soft}),
        _ => json!({"background": hexa(al, ac, ah, 0.13)}),
    };
    let outline = json!({
        "background": "#00000000", "radius": btn_r, "padding": btn_pad,
        "border": {"color": accent, "width": 1.5}, "text_color": accent,
        "transitions": transitions, "hover": outline_hover,
    });

    let checked_trans = if tms > 0.0 {
        json!([{"state": "checked", "duration_ms": tms.max(120.0), "easing": g.easing}])
    } else {
        json!([])
    };

    // the card mirrors the pane treatment: flat themes preview without an
    // outline, framed/strong ones keep it
    let card_border = match g.border.as_str() {
        "none" => json!(null),
        "strong" => json!({"color": edge, "width": 2.0}),
        _ => json!({"color": edge, "width": 1.0}),
    };
    json!({
        "card": {
            "background": pane, "radius": r_md,
            "border": card_border,
            "shadow": {"color": shadow_col, "blur": 16.0, "offset_y": 5.0},
        },
        "button_primary": primary,
        "button_secondary": secondary,
        "button_outline": outline,
        "toggle": {
            "track": {"background": s2, "radius": pill, "text_color": fg,
                      "transitions": checked_trans},
            "track_checked": {"background": accent, "radius": pill, "transitions": checked_trans},
            "knob": {"background": fg, "radius": pill},
            "knob_checked": {"background": primary_label, "radius": pill},
        },
        "checkbox": {
            "square": {"background": input, "radius": r_sm, "text_color": fg,
                       "border": {"color": edge, "width": 1.0}},
            "check": {"background": accent},
        },
        "radio": {
            "ring": {"background": input, "radius": pill, "text_color": fg,
                     "border": {"color": edge, "width": 1.0}},
            "dot": {"background": accent, "radius": pill},
        },
        "slider": {
            "track": {"background": s2, "radius": pill},
            "range": {"background": accent, "radius": pill},
            "thumb": {"background": fg, "radius": pill,
                      "shadow": {"color": shadow_col, "blur": 6.0, "offset_y": 2.0}},
        },
        "stepper": {
            "field": {"background": input, "radius": r_sm, "text_color": fg,
                      "border": {"color": edge, "width": 1.0}},
            "button": {"background": s2, "radius": r_sm, "text_color": accent,
                       "transitions": transitions, "hover": flat_hover(&s_hot)},
        },
        "select": {
            "trigger": {"background": input, "radius": r_sm, "text_color": fg,
                        "border": {"color": edge, "width": 1.0}},
            "menu": {"background": s1, "radius": r_md, "border": {"color": edge, "width": 1.0},
                     "shadow": {"color": shadow_col, "blur": 22.0, "offset_y": 8.0}},
            "item": {"text_color": muted},
            "item_selected": {"background": accent_soft, "text_color": fg},
        },
        "tabs": {
            "strip": {"background": s1, "radius": pill,
                      "padding": {"top": 2.0, "right": 2.0, "bottom": 2.0, "left": 2.0}},
            "tab": {"radius": pill, "text_color": muted,
                    "padding": {"top": 3.0, "right": 10.0, "bottom": 3.0, "left": 10.0}},
            "tab_selected": {"background": s3, "radius": pill, "text_color": fg,
                             "padding": {"top": 3.0, "right": 10.0, "bottom": 3.0, "left": 10.0}},
            "indicator": {"background": accent},
        },
        "input": {"background": input, "radius": r_sm, "text_color": fg,
                  "border": {"color": edge, "width": 1.0}},
        "table": {
            "panel": {"background": s1, "radius": r_md, "text_color": fg,
                      "border": {"color": edge, "width": 1.0}},
            "header": {"background": s2, "text_color": muted},
            "zebra": {"background": hexa(r.s2, sc, bh, 0.45)},
        },
        "bar": {
            "track": {"background": s2, "radius": pill},
            "fill": {"background": accent, "radius": pill},
        },
        "badge": {"background": accent, "radius": pill, "text_color": primary_label},
        "toast": {"surface": {"background": s3, "radius": r_md, "text_color": fg,
                              "border": {"color": edge, "width": 1.0},
                              "shadow": {"color": shadow_col, "blur": 18.0, "offset_y": 6.0}}},
        "popover": {
            "trigger": {"background": s2, "radius": r_sm, "text_color": fg,
                        "border": {"color": edge, "width": 1.0}},
            "surface": {"background": s1, "radius": r_md, "border": {"color": edge, "width": 1.0},
                        "shadow": {"color": shadow_col, "blur": 22.0, "offset_y": 8.0}},
        },
        "dialog": {
            "scrim": {"background": hexa(0.05, sc, bh, 0.55)},
            "panel": {"background": pane, "radius": r_lg, "border": {"color": edge, "width": 1.0},
                      "shadow": {"color": shadow_col, "blur": 30.0, "offset_y": 12.0}},
            "title": {"text_color": fg},
        },
        "tooltip": {"bubble": {"background": s3, "radius": r_sm, "text_color": fg,
                               "border": {"color": edge, "width": 1.0}}},
    })
}

// ---------------------------------------------------------- candidates

pub fn transition_ms(g: &Genome) -> f32 {
    match g.motion.as_str() {
        "instant" => 0.0,
        "smooth" => 260.0,
        _ => 130.0,
    }
}

/// Gradient stop colors for primary buttons, or None.
pub fn gradient_stops(g: &Genome) -> Option<[String; 2]> {
    let dark = g.mode != "light";
    let al = if dark { 0.74 } else { 0.55 };
    match g.gradient.as_str() {
        "subtle" => Some([
            hex(al + 0.05, g.accent_chroma, g.accent_hue),
            hex(al - 0.06, g.accent_chroma, g.accent_hue),
        ]),
        "bold" => Some([
            hex(al + 0.04, g.accent_chroma, (g.accent_hue - 18.0).rem_euclid(360.0)),
            hex(al - 0.05, g.accent_chroma * 1.1, (g.accent_hue + 22.0).rem_euclid(360.0)),
        ]),
        _ => None,
    }
}

pub fn candidate_json(g: &Genome) -> Value {
    let tokens: Map<String, Value> = expand_tokens(g).into_iter().collect();
    let id = format!("{:08x}", {
        // FNV-1a over the genome JSON — stable id for feedback joins
        let s = serde_json::to_string(g).expect("serialize genome");
        let mut h: u32 = 0x811c9dc5;
        for b in s.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x01000193);
        }
        h
    });
    json!({
        "id": id,
        "name": g.name,
        "slug": g.slug(),
        "genes": g,
        "genes_json": serde_json::to_string(g).expect("serialize genome"),
        "tokens": Value::Object(tokens),
        "ui": ui_styles(g),
        "preview": {
            "fx": preview_wgsl(g),
            "transition_ms": transition_ms(g),
            "easing": g.easing,
            "gradient": gradient_stops(g),
        },
    })
}

// --------------------------------------------------------------- adopt

/// Write the genome as a style preset: `~/.jim/styles/<name>/theme.ft`
/// (+ `chrome.wgsl`). Presets are discovered at app startup; the `lab`
/// preset exists from the start and hot-reloads, which is what Try Live
/// uses.
pub fn adopt(g: &Genome, preset: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var("HOME").expect("HOME not set");
    let dir = PathBuf::from(home).join(".jim").join("styles").join(preset);
    std::fs::create_dir_all(&dir)?;
    let mut out = String::new();
    out.push_str(&format!(
        "// {} — generated by style-muse ({} / {} / {} / {})\n{{\n",
        g.name, g.mode, g.harmony, g.depth, g.effect
    ));
    for (k, v) in expand_tokens(g) {
        match v {
            Value::String(s) => out.push_str(&format!("    {}: \"{}\",\n", k, s)),
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 {
                    out.push_str(&format!("    {}: {:.1},\n", k, f));
                } else {
                    out.push_str(&format!("    {}: {},\n", k, f));
                }
            }
            _ => {}
        }
    }
    out.push_str("}\n");
    std::fs::write(dir.join("theme.ft"), out)?;
    std::fs::write(dir.join("chrome.wgsl"), chrome_wgsl(g))?;
    Ok(dir)
}

// ----------------------------------------------------------------- next

/// Produce a batch. `mode`: "random" (sampler only), "llm" (DeepSeek,
/// error if unavailable), or "auto" (DeepSeek when a key exists and
/// there's enough feedback to be worth it, topped up with sampler picks).
pub fn next_batch(count: usize, mode: &str, note: &str) -> Value {
    let mut rng = Rng::from_clock();
    let model = taste_model();
    let feedback_n = model.0.events.len();

    let mut genomes: Vec<Genome> = Vec::new();
    let mut llm_error: Option<String> = None;

    let want_llm = match mode {
        "llm" => true,
        "auto" => LlmConfig::from_env().is_ok() && (feedback_n >= 4 || !note.is_empty()),
        _ => false,
    };
    if want_llm {
        let llm_count = if mode == "llm" { count } else { count.div_ceil(2) };
        match llm_genomes(llm_count, note, &mut rng) {
            Ok(mut gs) => genomes.append(&mut gs),
            Err(e) => llm_error = Some(e),
        }
    }
    if mode == "llm" && genomes.is_empty() {
        return json!({
            "error": format!(
                "DeepSeek batch failed: {}",
                llm_error.unwrap_or_else(|| "unknown".into())
            )
        });
    }
    while genomes.len() < count {
        let avoid: Vec<f32> = genomes.iter().map(|g| g.accent_hue).collect();
        genomes.push(sample_genome(&mut rng, &model, &avoid));
    }
    // batch-unique names (the word lists collide easily)
    let mut seen = std::collections::HashSet::new();
    for g in &mut genomes {
        for _ in 0..20 {
            if seen.insert(g.name.clone()) {
                break;
            }
            g.name = gen_name(&mut rng);
        }
    }

    let candidates: Vec<Value> = genomes.iter().map(candidate_json).collect();
    json!({
        "candidates": candidates,
        "feedback_count": feedback_n,
        "source": if want_llm && llm_error.is_none() { if mode == "llm" { "llm" } else { "llm+random" } } else { "random" },
        "llm_error": llm_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_gamut_safe_everywhere() {
        let mut rng = Rng(42);
        for _ in 0..500 {
            let h = hex(rng.f32(), rng.f32() * 0.4, rng.f32() * 360.0);
            assert_eq!(h.len(), 7);
            assert!(h.starts_with('#'));
        }
    }

    #[test]
    fn known_color_roundtrip() {
        // oklch(1, 0, _) is white; oklch(0, 0, _) is black
        assert_eq!(hex(1.0, 0.0, 0.0), "#ffffff");
        assert_eq!(hex(0.0, 0.0, 0.0), "#000000");
    }

    #[test]
    fn expand_covers_required_tokens() {
        let mut rng = Rng(7);
        let model = taste_model();
        let g = sample_genome(&mut rng, &model, &[]);
        let tokens = expand_tokens(&g);
        let keys: Vec<&str> = tokens.iter().map(|(k, _)| k.as_str()).collect();
        for required in [
            "pane_bg", "bg", "fg", "accent", "syntax_keyword", "button_primary_bg",
            "canvas_bg", "sidebar_bg", "surface_2", "accent_500", "radius_md",
            "pane_focus_strength", "status_failed", "widget_bar_fill",
        ] {
            assert!(keys.contains(&required), "missing token {}", required);
        }
    }

    #[test]
    fn focus_stays_neutral() {
        // even a screaming-orange accent must not leak into focus tokens
        let mut g = Genome {
            name: "Test".into(),
            mode: "dark".into(),
            base_hue: 60.0,
            surface_chroma: 0.03,
            accent_hue: 60.0,
            accent_chroma: 0.23,
            harmony: "mono".into(),
            contrast: "high".into(),
            radius: 6.0,
            border: "hairline".into(),
            depth: "glow".into(),
            gradient: "bold".into(),
            motion: "snappy".into(),
            easing: "ease_out".into(),
            effect: "aurora".into(),
            effect_strength: 0.3,
            hover_style: "glow".into(),
        };
        let mut rng = Rng(1);
        g.sanitize(&mut rng);
        let tokens: Map<String, Value> = expand_tokens(&g).into_iter().collect();
        // focus must be QUIET: the border never changes with focus (no
        // bright outline) — the only focus cue is the title/margin ring
        // lightening one step
        assert_eq!(
            tokens["pane_border_focused"], tokens["pane_border"],
            "focused border must equal resting border"
        );
        assert_eq!(tokens["pane_border_width_focused"], tokens["pane_border_width"]);
        assert_ne!(
            tokens["chrome_title_bg_focused"], tokens["chrome_title_bg"],
            "ring must shift a step on focus"
        );
        assert_eq!(tokens["pane_focus_strength"], json!(0.0));
    }

    #[test]
    fn sanitize_fixes_llm_garbage() {
        let mut g: Genome = serde_json::from_str(
            r#"{"name":"","mode":"midnight","base_hue":-90,"accent_hue":725,
                "accent_chroma":9.0,"harmony":"wat","contrast":"señor",
                "radius":99,"border":"x","depth":"y","gradient":"z",
                "motion":"w","easing":"v","effect":"u","effect_strength":7}"#,
        )
        .unwrap();
        let mut rng = Rng(3);
        g.sanitize(&mut rng);
        assert!(MODES.contains(&g.mode.as_str()));
        assert!((0.0..360.0).contains(&g.accent_hue));
        assert!(g.radius <= 16.0);
        assert!(!g.name.is_empty());
    }

    #[test]
    fn chrome_wgsl_static_for_none() {
        let mut rng = Rng(9);
        let model = taste_model();
        let mut g = sample_genome(&mut rng, &model, &[]);
        g.effect = "none".into();
        assert!(!chrome_wgsl(&g).contains("params.time"));
        g.effect = "aurora".into();
        assert!(chrome_wgsl(&g).contains("params.time"));
    }
}
