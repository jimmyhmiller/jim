# Waveform rendering — design notes & POC findings

## The complaint

In the podcast widget's waveform strip, zooming makes the loud/quiet **contour
disappear** — "everything just becomes a flat block / big blob," especially when
zoomed out. The user wants to see structure (loud passages vs pauses) to find
edit points, at every zoom.

## What the data actually is

`~/.jim/transcripts/<name>.peaks.json` = `{ bucket_ms: 10, peaks: [f32; N] }`.
Each `peaks[i]` is the **Peak_level (max abs sample)** of one 10 ms bucket,
extracted by ffmpeg `astats`, then in `parse_peaks` converted dBFS→linear
amplitude (`10^(dB/20)`) and normalized so the loudest bucket = 1.0.

`slop_stereo.m4a`: N = 137 823 buckets (1378 s). Distribution:

```
min 0.000  max 0.992  mean 0.460  median 0.519
0.0-0.1: 17226  ██████      (≈12.5% near-silence — the pauses)
0.1-0.5: 48919  ████████████████
0.5-0.8: 65042  ████████████████████████  (most of it — loud, compressed)
0.8-1.0:  6236  ██
```

This is **loudness-compressed** audio: it's loud most of the time. That single
fact drives everything below.

## Why each naive approach blobs

A pixel column at fit zoom spans ~98 buckets (~1 s of audio).

- **MAX per column** (the original): max over ~98 buckets almost always hits a
  loud bucket → every column ≈ full height → solid block. Worst offender.
- **RMS per column** (`sqrt(mean square)`): mean of mostly-loud buckets ≈ 0.5,
  so columns cluster in 0.4–0.7 → a flat half-height band. Still a blob, just a
  shorter one. (Measured fit-zoom column histogram clustered hard at 0.5–0.6.)
- **Subsampled RMS** (the stride hack that scanned ~8 of every ~98 buckets to
  stay cheap): *aliases*. Whether a column reads loud or quiet depends on which
  buckets the stride happens to land on, so a single loud bucket in a quiet
  stretch spikes a whole column, and as you zoom the stride realigns and the
  spikes wash the contour into noise. The "contour" this showed was largely
  sampling noise, not signal.
- **dB / log mappings** with a fixed floor (e.g. −48 dB): RMS ≈ 0.5 → −6 dB →
  ~0.87 of full height → saturates to a block again.

## What works (proven by the POC)

Render `crates/.../waveform-poc` produces `/tmp/wf_{fit,zoom4x,zoom16x}.png`,
each stacking 7 methods. At **fit zoom** only two avoid the blob:

- **ENERGY (mean-square) per column, normalized to the loudest visible column.**
  Squaring spreads the compressed midrange (quiet dips fall away, loud passages
  stand up); per-view normalization keeps the strip fully used at any zoom.
  → clear, readable contour at fit AND fine detail when zoomed in.
- **ENERGY normalized to the 95th-percentile column** — same, but robust against
  a single loud transient setting the scale. Needs a sort (or a select) per
  build; `maxN` doesn't and looked equally good on real data, so `maxN` is the
  default.

Decision: **per-column energy = mean of peak² over every bucket the column
covers, normalized per view.** Exactly, not subsampled.

### Making it exact AND cheap

Subsampling was a perf hack (don't scan 100k+ buckets per pan). Replace it with a
**prefix-sum-of-squares** built once when peaks load:

```
ss[k] = Σ peaks[0..k]²            (len N+1, ss[0]=0)
column energy over [b0,b1) = (ss[b1] - ss[b0]) / (b1 - b0)   // O(1)
```

So `build_cols` is O(visible pixels ≈ 1300), exact at every zoom, cheap on
pan/zoom regardless of file length. `prefix_sumsq` is O(N) once per load
(measured: 138k in ~0.05 s in the funct VM; `push` is amortized O(1)).

State plumbing: `sumsq` is cached in widget state alongside `peaks`, rebuilt
wherever `peaks` changes, and `recols_state` self-heals (via `get`, not field
access, so a snapshot restored from before the field existed doesn't fault) if
`len(sumsq) != len(peaks)+1`.

## Tradeoff to remember

Per-view normalization means the waveform is **relative**, not absolute: a
uniformly-quiet section, zoomed into, is stretched to full height (its loudest
bit reaches the top). That's the right call for *finding structure to edit*, but
it is not an absolute amplitude display. p95 normalization over ~1300 columns is
stable enough that panning doesn't visibly "breathe."

## The live-deploy gotcha (the real reason it "kept not working")

The algorithm above was correct in the widget the whole time — the POC renders it
correctly offline. The live failures were **deploy/caching**, not math:

1. `.ft` hot-reload from the `~/.jim/widgets` symlink did not reliably pick up
   edits, so the running widget kept executing older code.
2. Even after the new code loads, `render` reads the cached `wave_cols`. That
   cache only rebuilds on load / resize / zoom / pan / select — and on
   reload/restart it is **restored stale from the persisted snapshot**. So the
   strip shows old (MAX-era) bars until a rebuild is forced.

**Reliable deploy:** full `./scripts/dev-restart.sh` (guarantees the new `.ft` is
read), then re-select the recording (forces `recache` → `build_cols`). Forcing a
rebuild over the bus (`topic:podcast.open {name}`) also works once the new code
is actually loaded.

## Files

- `waveform-poc/` — standalone Rust crate (detached from the workspace). Loads a
  real `peaks.json`, renders the 7-method comparison PNGs. `cargo run --release`.
- Widget impl: `crates/jim-widget/widgets/podcast.ft` — `prefix_sumsq`,
  `build_cols`, `recols_state`.
