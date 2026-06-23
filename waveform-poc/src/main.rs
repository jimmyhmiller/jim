// Waveform rendering POC — offline, deterministic.
//
// Loads a real jim peaks.json (Peak_level per 10ms bucket, 0..1) and renders the
// waveform strip to PNG with several candidate column-aggregation algorithms,
// stacked, at several zoom levels — so we can SEE which one shows readable
// loud/quiet contour instead of a flat block, without the live widget / hot
// reload / screenshot loop.
//
// Usage: cargo run --release -- [peaks.json] [out_dir]
// Default peaks: ~/.jim/transcripts/slop_stereo.m4a.peaks.json
//
// Each output file `wf_<zoom>.png` stacks the methods top→bottom in this order
// (printed at runtime too):
//   0 MAX            max peak per column (the original; saturates on zoom-out)
//   1 RMS            sqrt(mean square) per column, exact
//   2 ENERGY-maxN    mean-square per column, normalized to loudest visible col
//   3 ENERGY-p95N    mean-square per column, normalized to 95th-pct visible col
//   4 RMS-dB         20log10(rms) mapped from [FLOOR_DB,0] dB (perceptual)
//   5 PEAK-maxN      max peak per column, normalized to loudest visible col
//   6 RMS-dB-p95     dB but the 0-dB ref is the 95th-pct col (auto-gain)

use image::{Rgb, RgbImage};
use std::fs;

const W: u32 = 1300; // strip width in px (columns)
const STRIP_H: u32 = 150; // per-method strip height
const GAP: u32 = 14;
const PAD: f64 = 6.0; // vertical padding inside a strip
const FLOOR_DB: f64 = -48.0;

const BG: Rgb<u8> = Rgb([18, 20, 26]);
const STRIP_BG: Rgb<u8> = Rgb([12, 14, 18]);
const BAR: Rgb<u8> = Rgb([63, 127, 174]); // #3f7fae
const SEP: Rgb<u8> = Rgb([40, 44, 54]);

fn load_peaks(path: &str) -> Vec<f64> {
    let txt = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
    v["peaks"]
        .as_array()
        .expect("peaks array")
        .iter()
        .map(|x| x.as_f64().unwrap_or(0.0))
        .collect()
}

// For a view over buckets [start, start+span) across W columns, return the
// [b0,b1) bucket range each column covers (b1 > b0 guaranteed within bounds).
fn col_ranges(n: usize, start: f64, span: f64) -> Vec<(usize, usize)> {
    let step = span / W as f64;
    (0..W)
        .filter_map(|i| {
            let b0 = (start + i as f64 * step).floor().max(0.0) as usize;
            let mut b1 = (start + (i as f64 + 1.0) * step).ceil() as usize;
            if b1 > n {
                b1 = n;
            }
            let b0 = b0.min(n);
            if b1 > b0 {
                Some((b0, b1))
            } else {
                None
            }
        })
        .collect()
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 1e-9;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[idx].max(1e-9)
}

// ---- the candidate methods. each returns one value in [0,1] per column. ----

fn m_max(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    r.iter()
        .map(|&(a, b)| p[a..b].iter().cloned().fold(0.0, f64::max))
        .collect()
}

fn m_rms(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    r.iter()
        .map(|&(a, b)| {
            let s: f64 = p[a..b].iter().map(|x| x * x).sum();
            (s / (b - a) as f64).sqrt()
        })
        .collect()
}

fn energy(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    r.iter()
        .map(|&(a, b)| {
            let s: f64 = p[a..b].iter().map(|x| x * x).sum();
            s / (b - a) as f64
        })
        .collect()
}

fn normalize_to(vals: &[f64], reference: f64) -> Vec<f64> {
    vals.iter().map(|&v| (v / reference).min(1.0)).collect()
}

fn m_energy_maxn(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    let e = energy(p, r);
    let mx = e.iter().cloned().fold(1e-9, f64::max);
    normalize_to(&e, mx)
}

fn m_energy_p95n(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    let e = energy(p, r);
    let ref95 = percentile(e.clone(), 0.95);
    normalize_to(&e, ref95)
}

fn m_peak_maxn(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    let mx = m_max(p, r);
    let m = mx.iter().cloned().fold(1e-9, f64::max);
    normalize_to(&mx, m)
}

fn db_map(rms: &[f64], ref0: f64) -> Vec<f64> {
    rms.iter()
        .map(|&v| {
            let db = 20.0 * (v.max(1e-6) / ref0).log10();
            ((db - FLOOR_DB) / (0.0 - FLOOR_DB)).clamp(0.0, 1.0)
        })
        .collect()
}

fn m_rms_db(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    db_map(&m_rms(p, r), 1.0)
}

fn m_rms_db_p95(p: &[f64], r: &[(usize, usize)]) -> Vec<f64> {
    let rms = m_rms(p, r);
    let ref0 = percentile(rms.clone(), 0.95);
    db_map(&rms, ref0)
}

fn methods() -> Vec<(&'static str, fn(&[f64], &[(usize, usize)]) -> Vec<f64>)> {
    vec![
        ("MAX", m_max),
        ("RMS", m_rms),
        ("ENERGY-maxN", m_energy_maxn),
        ("ENERGY-p95N", m_energy_p95n),
        ("RMS-dB", m_rms_db),
        ("PEAK-maxN", m_peak_maxn),
        ("RMS-dB-p95", m_rms_db_p95),
    ]
}

fn draw_strip(img: &mut RgbImage, y0: u32, ranges: &[(usize, usize)], vals: &[f64]) {
    // strip background
    for y in y0..y0 + STRIP_H {
        for x in 0..W {
            img.put_pixel(x, y, STRIP_BG);
        }
    }
    let mid = y0 as f64 + STRIP_H as f64 / 2.0;
    let max_half = STRIP_H as f64 / 2.0 - PAD;
    // ranges and vals are 1:1; column x index = position in the kept list.
    for (i, &v) in vals.iter().enumerate() {
        let x = i as u32; // ranges were built per column 0..W (filter kept order)
        if x >= W {
            break;
        }
        let half = (v.clamp(0.0, 1.0) * max_half).max(0.5);
        let top = (mid - half).floor() as i64;
        let bot = (mid + half).ceil() as i64;
        for y in top..bot {
            if y >= 0 && (y as u32) < img.height() {
                img.put_pixel(x, y as u32, BAR);
            }
        }
        let _ = ranges; // ranges kept for parity / future min-max use
    }
}

fn render_zoom(p: &[f64], label: &str, start: f64, span: f64, out_dir: &str) {
    let ms = methods();
    let total_h = ms.len() as u32 * (STRIP_H + GAP) + GAP;
    let mut img = RgbImage::from_pixel(W, total_h, BG);
    let ranges = col_ranges(p.len(), start, span);
    let mut y = GAP;
    for (_name, f) in &ms {
        // separator line above strip
        for x in 0..W {
            img.put_pixel(x, y - 1, SEP);
        }
        let vals = f(p, &ranges);
        draw_strip(&mut img, y, &ranges, &vals);
        y += STRIP_H + GAP;
    }
    let path = format!("{out_dir}/wf_{label}.png");
    img.save(&path).unwrap();
    println!("wrote {path}  ({} cols)", ranges.len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let home = std::env::var("HOME").unwrap();
    let peaks_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| format!("{home}/.jim/transcripts/slop_stereo.m4a.peaks.json"));
    let out_dir = args.get(2).cloned().unwrap_or_else(|| "/tmp".to_string());

    let p = load_peaks(&peaks_path);
    let n = p.len();
    println!("loaded {} buckets ({:.1}s) from {}", n, n as f64 * 0.01, peaks_path);
    println!("method stack order (top→bottom):");
    for (i, (name, _)) in methods().iter().enumerate() {
        println!("  {i} {name}");
    }

    let nf = n as f64;
    // fit = whole file; zoom k shows the FIRST n/k buckets (start at 0).
    render_zoom(&p, "fit", 0.0, nf, &out_dir);
    render_zoom(&p, "zoom4x", 0.0, nf / 4.0, &out_dir);
    render_zoom(&p, "zoom16x", 0.0, nf / 16.0, &out_dir);
}
