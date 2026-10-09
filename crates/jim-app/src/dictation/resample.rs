//! Streaming sample-rate conversion from the microphone's native rate to
//! the 16 kHz both transcription engines want.
//!
//! The microphone delivers audio in whatever chunk sizes the device
//! callback produced, so this has to be *stateful*: a filter that restarted
//! at every chunk boundary would click there. [`Resampler::process`] keeps
//! the input history its filter needs and produces exactly the samples a
//! single pass over the whole clip would have — the `chunking_is_invisible`
//! test pins that down.
//!
//! It is a windowed-sinc low-pass evaluated at each output instant. Dropping
//! from 48 kHz to 16 kHz without one folds everything between 8 and 24 kHz
//! (fans, keyboard clatter, sibilance) back down into the speech band; a
//! plain "take every third sample" decimator does exactly that.
//!
//! Rates are integers, so output instant `n` sits at input position
//! `n · in / out`, whose fractional part only ever takes `out / gcd` distinct
//! values. Each of those phases gets a precomputed, DC-normalised kernel, so
//! the per-sample work is one dot product — 48k → 16k has a single phase.

/// Zero crossings of the sinc on each side of the centre. Sets the
/// transition band: 16 gives a steep cutoff for ~100 taps at 48k → 16k.
const ZERO_CROSSINGS: f64 = 16.0;
/// Fraction of the output Nyquist frequency the passband keeps. Below 1 so
/// the transition band sits under the new Nyquist rather than straddling it.
const ROLLOFF: f64 = 0.92;
/// Above this many phases the kernels are computed per sample instead of
/// tabulated. Only reached by unusual rate pairs.
const MAX_TABLE_PHASES: u64 = 4096;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Taps either side of the centre, in input samples.
    half: usize,
    /// Cutoff in cycles per INPUT sample.
    cutoff: f64,
    /// One kernel per fractional phase (`2 * half` taps each), or empty
    /// when there are too many phases to tabulate.
    table: Vec<Vec<f32>>,
    /// `out / gcd`: how many distinct phases there are.
    phases: u64,
    /// Input samples not yet out of the filter's reach.
    buf: Vec<f32>,
    /// Absolute input index of `buf[0]`.
    buf_start: u64,
    /// Input samples pushed so far.
    total_in: u64,
    /// Output samples produced so far.
    produced: u64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        assert!(in_rate > 0 && out_rate > 0, "sample rates must be positive");
        let g = gcd(in_rate as u64, out_rate as u64);
        let phases = out_rate as u64 / g;
        // Low-pass at the lower of the two Nyquists, as cycles per input sample.
        let cutoff = 0.5 * (out_rate as f64 / in_rate as f64).min(1.0) * ROLLOFF;
        let half = (ZERO_CROSSINGS / (2.0 * cutoff)).ceil() as usize;
        let mut r = Resampler {
            in_rate,
            out_rate,
            half,
            cutoff,
            table: Vec::new(),
            phases,
            buf: Vec::new(),
            buf_start: 0,
            total_in: 0,
            produced: 0,
        };
        if in_rate != out_rate && phases <= MAX_TABLE_PHASES {
            r.table = (0..phases)
                .map(|p| r.kernel(p as f64 / phases as f64))
                .collect();
        }
        r
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Resample the next chunk. Output lags input by `half` input samples —
    /// the filter needs that much look-ahead; [`Self::flush`] releases it.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.in_rate == self.out_rate {
            self.total_in += input.len() as u64;
            self.produced += input.len() as u64;
            return input.to_vec();
        }
        self.buf.extend_from_slice(input);
        self.total_in += input.len() as u64;
        // Everything whose right-hand taps are now available.
        let available = self.total_in;
        self.drain(available)
    }

    /// Emit everything still held back, treating the input as followed by
    /// silence. The resampler is spent afterwards.
    pub fn flush(&mut self) -> Vec<f32> {
        if self.in_rate == self.out_rate {
            return Vec::new();
        }
        let end = self.total_in;
        let pad = self.half as u64 + 1;
        self.buf.extend(std::iter::repeat_n(0.0, pad as usize));
        let out_total = (end * self.out_rate as u64).div_ceil(self.in_rate as u64);
        let mut out = self.drain(end + pad);
        let excess = (self.produced).saturating_sub(out_total) as usize;
        out.truncate(out.len().saturating_sub(excess));
        self.produced = self.produced.min(out_total);
        out
    }

    /// Produce every output sample whose last tap is below `limit`.
    fn drain(&mut self, limit: u64) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            let num = self.produced * self.in_rate as u64;
            let centre = num / self.out_rate as u64;
            let last_tap = centre + self.half as u64;
            if last_tap >= limit {
                break;
            }
            let rem = num % self.out_rate as u64;
            let frac = rem as f64 / self.out_rate as f64;
            let first_tap = centre as i64 - self.half as i64 + 1;
            let y = if self.table.is_empty() {
                let k = self.kernel(frac);
                self.dot(first_tap, &k)
            } else {
                // rem is a multiple of gcd, so this is the exact phase.
                let phase = (rem * self.phases / self.out_rate as u64) as usize;
                let k = &self.table[phase];
                self.dot(first_tap, k)
            };
            out.push(y);
            self.produced += 1;
        }
        // Keep only what the next output's taps can still reach.
        let next_centre = self.produced * self.in_rate as u64 / self.out_rate as u64;
        let keep_from = (next_centre + 1).saturating_sub(self.half as u64);
        if keep_from > self.buf_start {
            let drop = ((keep_from - self.buf_start) as usize).min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
        out
    }

    /// Taps at input offsets `-half+1 ..= half` around a centre `frac`
    /// samples before the true output instant, normalised to unit DC gain.
    fn kernel(&self, frac: f64) -> Vec<f32> {
        let h = self.half as f64;
        let mut k: Vec<f64> = (0..2 * self.half)
            .map(|i| {
                let x = (i as f64 - h + 1.0) - frac;
                let arg = 2.0 * self.cutoff * x;
                let sinc = if arg.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg)
                };
                // Blackman over [-h, h]: ~-74 dB sidelobes.
                let w = if x.abs() >= h {
                    0.0
                } else {
                    let t = (x + h) / (2.0 * h);
                    0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()
                        + 0.08 * (4.0 * std::f64::consts::PI * t).cos()
                };
                sinc * w
            })
            .collect();
        let sum: f64 = k.iter().sum();
        if sum.abs() > 1e-12 {
            for v in &mut k {
                *v /= sum;
            }
        }
        k.into_iter().map(|v| v as f32).collect()
    }

    /// Dot product of the kernel against input starting at absolute index
    /// `first`; indices before the clip began read as silence.
    fn dot(&self, first: i64, k: &[f32]) -> f32 {
        let mut acc = 0.0f32;
        for (i, &c) in k.iter().enumerate() {
            let idx = first + i as i64;
            if idx < self.buf_start as i64 {
                // Before the clip, or already dropped — dropped samples are
                // never reachable by construction, so this is the pre-roll.
                continue;
            }
            let j = (idx - self.buf_start as i64) as usize;
            if let Some(&x) = self.buf.get(j) {
                acc += x * c;
            }
        }
        acc
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp)
            .collect()
    }

    fn all(r: &mut Resampler, x: &[f32]) -> Vec<f32> {
        let mut out = r.process(x);
        out.extend(r.flush());
        out
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Zero crossings per second ≈ 2 × frequency.
    fn freq(x: &[f32], rate: u32) -> f32 {
        let crossings = x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        crossings as f32 * rate as f32 / (2.0 * x.len() as f32)
    }

    #[test]
    fn same_rate_is_a_passthrough() {
        let x = tone(16_000, 440.0, 0.1, 0.5);
        let mut r = Resampler::new(16_000, 16_000);
        assert_eq!(all(&mut r, &x), x);
    }

    #[test]
    fn output_length_matches_the_rate_ratio() {
        for (inr, n) in [(48_000u32, 48_000usize), (44_100, 44_100), (48_000, 12_345)] {
            let mut r = Resampler::new(inr, 16_000);
            let out = all(&mut r, &vec![0.1; n]);
            let want = (n as u64 * 16_000).div_ceil(inr as u64) as usize;
            assert_eq!(out.len(), want, "{inr} Hz, {n} samples");
        }
    }

    #[test]
    fn dc_survives_at_unit_gain() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = all(&mut r, &vec![0.5; 48_000]);
        // Ignore the filter's ramp at either edge.
        for &v in &out[200..out.len() - 200] {
            assert!((v - 0.5).abs() < 1e-3, "DC came out as {v}");
        }
    }

    #[test]
    fn speech_band_tone_keeps_pitch_and_level() {
        for inr in [48_000u32, 44_100] {
            let mut r = Resampler::new(inr, 16_000);
            let out = all(&mut r, &tone(inr, 1_000.0, 1.0, 0.5));
            let mid = &out[1_000..out.len() - 1_000];
            let f = freq(mid, 16_000);
            assert!((f - 1_000.0).abs() < 5.0, "{inr} Hz: pitch moved to {f}");
            let level = rms(mid);
            assert!((level - 0.5 / 2f32.sqrt()).abs() < 0.01, "{inr} Hz: level {level}");
        }
    }

    /// The reason this isn't "take every third sample": a 10 kHz tone has
    /// no place in 16 kHz audio and must not alias down into the speech band.
    #[test]
    fn content_above_the_new_nyquist_is_removed() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = all(&mut r, &tone(48_000, 10_000.0, 1.0, 0.5));
        let level = rms(&out[200..out.len() - 200]);
        assert!(level < 0.003, "10 kHz leaked through at rms {level}");
    }

    /// Chunk boundaries must not exist in the output: any split of the
    /// input produces the same samples as one push.
    #[test]
    fn chunking_is_invisible() {
        for inr in [48_000u32, 44_100] {
            let x = tone(inr, 523.0, 0.7, 0.4);
            let whole = all(&mut Resampler::new(inr, 16_000), &x);
            let mut r = Resampler::new(inr, 16_000);
            let mut pieces = Vec::new();
            let sizes = [1usize, 7, 480, 13, 1024, 3, 999];
            let mut i = 0;
            let mut s = 0;
            while i < x.len() {
                let n = sizes[s % sizes.len()].min(x.len() - i);
                pieces.extend(r.process(&x[i..i + n]));
                i += n;
                s += 1;
            }
            pieces.extend(r.flush());
            assert_eq!(pieces.len(), whole.len(), "{inr} Hz length");
            for (a, b) in pieces.iter().zip(&whole) {
                assert!((a - b).abs() < 1e-6, "{inr} Hz: chunked {a} vs whole {b}");
            }
        }
    }
}
