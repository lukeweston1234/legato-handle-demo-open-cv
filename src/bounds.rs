//! Which slice of the sample each finger plays, and how it snaps to the beat.
//!
//! The viewer edits these live through the slicer's handle, and saves them
//! next to the sample so a tuned sample stays tuned.

use std::{
    fmt::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use crate::gesture::PADS;

/// One finger's slice of the sample, as fractions of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slice {
    pub start: f32,
    pub end: f32,
}

impl Slice {
    pub fn len(&self) -> f32 {
        self.end - self.start
    }
}

/// The four slices the fingers play, and the view and grid they are set on.
///
/// The slices are the source of truth: the viewer places them, and the
/// hands only choose which one plays and how much of it. Everything is in
/// fractions of the sample or in ms, so it doesn't change with sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// The span the viewer zooms in on and lays the grid over.
    pub start: f32,
    pub end: f32,
    /// Grid lines across the view. 0 means no grid.
    pub slices: u32,
    /// Snap slice edges to the grid, and pinched lengths to halves, quarters…
    pub snap: bool,
    /// The shortest a pinch can cut a slice down to.
    pub min_len_ms: f32,
    pub pads: [Slice; PADS],
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            start: 0.0,
            end: 1.0,
            slices: 16,
            snap: true,
            min_len_ms: 20.0,
            pads: [Slice {
                start: 0.0,
                end: 0.0,
            }; PADS],
        }
        .spread()
    }
}

impl Bounds {
    /// Ordered, inside the sample, and not empty.
    pub fn sanitized(mut self) -> Self {
        let ordered = |a: &mut f32, b: &mut f32| {
            *a = a.clamp(0.0, 1.0);
            *b = b.clamp(0.0, 1.0);
            if *b < *a {
                std::mem::swap(a, b);
            }
        };
        ordered(&mut self.start, &mut self.end);
        for pad in &mut self.pads {
            ordered(&mut pad.start, &mut pad.end);
        }
        self.slices = self.slices.min(256);
        self.min_len_ms = self.min_len_ms.max(0.1);
        self
    }

    /// The grid step, as a fraction of the sample, if there is a grid.
    pub fn step(&self) -> Option<f32> {
        (self.slices > 0).then(|| (self.end - self.start) / self.slices as f32)
    }

    /// `f` moved to the nearest grid line, when snapping.
    pub fn snap_point(&self, f: f32) -> f32 {
        match self.step().filter(|&s| self.snap && s > 0.0) {
            Some(step) => (self.start + ((f - self.start) / step).round() * step).clamp(0.0, 1.0),
            None => f.clamp(0.0, 1.0),
        }
    }

    /// The pads laid side by side from the start of the view: one grid step
    /// each, or a quarter of the view each without a grid.
    pub fn spread(mut self) -> Self {
        let width = self.step().unwrap_or((self.end - self.start) / PADS as f32);
        for (n, pad) in self.pads.iter_mut().enumerate() {
            let start = (self.start + n as f32 * width).min(1.0);
            *pad = Slice {
                start,
                end: (start + width).min(1.0),
            };
        }
        self
    }

    /// Where `pad` starts, in samples of a `total`-sample buffer.
    pub fn pad_start(&self, pad: usize, total: f32) -> f32 {
        (self.pads[pad].start * total).round()
    }

    /// How much of `pad` plays, in samples, for a `len` control (0..1).
    ///
    /// Open plays the whole slice, pinched cuts it to `min_len`. Snapped, it
    /// steps through halves, quarters, eighths of the slice, so a pinch
    /// stutters in time rather than drifting out of it.
    pub fn play_len(&self, pad: usize, len: f32, sample_rate: f32, total: f32) -> f32 {
        let pad = self.pads[pad];
        // Edges rounded to whole samples, as `pad_start` does.
        let full = ((pad.end * total).round() - (pad.start * total).round()).max(2.0);
        let min = (self.min_len_ms * sample_rate * 0.001).clamp(2.0, full);
        let len = len.clamp(0.0, 1.0);
        if self.snap {
            let halvings = (full / min).log2().floor().max(0.0);
            full / ((1.0 - len) * halvings).round().exp2()
        } else {
            min * (full / min).powf(len)
        }
    }
}

/// [`Bounds`] plus what only the viewer cares about, as saved to disk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub bounds: Bounds,
    /// How many beats the view spans, which sets its tempo.
    pub beats: f32,
    /// Retune the clock to the view's tempo, so the grid sync follows the sample.
    pub follow_clock: bool,
}

impl Preset {
    /// Defaults for a sample `secs` long, guessing a beat count that puts it
    /// between 80 and 160 BPM, the way most loops are cut.
    pub fn guess(bounds: Bounds, secs: f32) -> Self {
        let mut beats = 4.0;
        while secs > 0.0 && 60.0 * beats / secs < 80.0 && beats < 256.0 {
            beats *= 2.0;
        }
        while 60.0 * beats / secs > 160.0 && beats > 1.0 {
            beats /= 2.0;
        }
        Self {
            bounds,
            beats,
            follow_clock: true,
        }
    }

    /// The view's tempo, for a sample `secs` long.
    pub fn bpm(&self, secs: f32) -> f32 {
        let window = (self.bounds.end - self.bounds.start) * secs;
        if window > 0.0 {
            60.0 * self.beats / window
        } else {
            0.0
        }
    }

    /// `amen.wav` saves to `amen.wav.slices`.
    pub fn path_for(sample: &Path) -> PathBuf {
        let mut name = sample.as_os_str().to_owned();
        name.push(".slices");
        PathBuf::from(name)
    }

    pub fn to_text(&self) -> String {
        let b = &self.bounds;
        let mut text = format!(
            "# Marionette slices, written by the viewer.\n\
             start = {}\nend = {}\nslices = {}\nsnap = {}\nmin_len = {}\n\
             beats = {}\nfollow_clock = {}\n",
            b.start, b.end, b.slices, b.snap, b.min_len_ms, self.beats, self.follow_clock
        );
        for (n, pad) in b.pads.iter().enumerate() {
            let _ = writeln!(text, "pad{n} = {} {}", pad.start, pad.end);
        }
        text
    }

    /// Reads what [`Self::to_text`] wrote over `self`. Unknown keys are an
    /// error so a typo doesn't silently do nothing. A file from before the
    /// pads were saved lays them out across its view.
    pub fn from_text(mut self, text: &str) -> Result<Self, String> {
        let mut has_pads = false;
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let err = |what: &str| format!("line {}: {what}: '{line}'", n + 1);
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| err("expected key = value"))?;
            let value = value.trim();
            let num = || value.parse::<f32>().map_err(|_| err("not a number"));
            let flag = || value.parse::<bool>().map_err(|_| err("not true/false"));
            let b = &mut self.bounds;
            match key.trim() {
                "start" => b.start = num()?,
                "end" => b.end = num()?,
                "slices" => b.slices = value.parse().map_err(|_| err("not a count"))?,
                "snap" => b.snap = flag()?,
                "min_len" => b.min_len_ms = num()?,
                // The open length is now the slice itself.
                "max_len" => {}
                "beats" => self.beats = num()?.max(0.25),
                "follow_clock" => self.follow_clock = flag()?,
                key => {
                    let pad = key
                        .strip_prefix("pad")
                        .and_then(|n| n.parse::<usize>().ok())
                        .filter(|&n| n < PADS)
                        .ok_or_else(|| err("unknown key"))?;
                    let edges: Vec<f32> = value
                        .split_whitespace()
                        .map(|v| v.parse().map_err(|_| err("not a number")))
                        .collect::<Result<_, _>>()?;
                    let [start, end] = edges[..] else {
                        return Err(err("expected start and end"));
                    };
                    b.pads[pad] = Slice { start, end };
                    has_pads = true;
                }
            }
        }
        if !has_pads {
            self.bounds = self.bounds.sanitized().spread();
        }
        self.bounds = self.bounds.sanitized();
        Ok(self)
    }
}

/// An `f32` that can be shared between threads without a lock.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// What the slicer is doing, for the viewer to draw. Fractions of the sample.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Playhead {
    /// How much of the sounding slice plays, after pinching.
    pub len: f32,
    /// The slice that is sounding, if any.
    pub playing: Option<Playing>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Playing {
    pub pad: usize,
    pub start: f32,
    pub at: f32,
}

/// Lock-free state shared between the slicer and the viewer. Fields are read
/// and written one at a time, so a reader may see half an update; for a
/// handful of knobs and a playhead redrawn every frame, that's fine.
#[derive(Debug, Default)]
pub struct Shared {
    start: AtomicF32,
    end: AtomicF32,
    slices: AtomicU32,
    snap: AtomicBool,
    min_len_ms: AtomicF32,
    pads: [[AtomicF32; 2]; PADS],
    /// Bumped by the viewer to play a pad without a hand.
    auditions: [AtomicU32; PADS],

    len: AtomicF32,
    playing: AtomicBool,
    pad: AtomicU32,
    voice_start: AtomicF32,
    at: AtomicF32,
}

impl Shared {
    pub fn new(bounds: Bounds) -> Self {
        let shared = Self::default();
        shared.set_bounds(bounds);
        shared
    }

    pub fn bounds(&self) -> Bounds {
        Bounds {
            start: self.start.get(),
            end: self.end.get(),
            slices: self.slices.load(Ordering::Relaxed),
            snap: self.snap.load(Ordering::Relaxed),
            min_len_ms: self.min_len_ms.get(),
            pads: std::array::from_fn(|n| Slice {
                start: self.pads[n][0].get(),
                end: self.pads[n][1].get(),
            }),
        }
        .sanitized()
    }

    pub fn set_bounds(&self, b: Bounds) {
        let b = b.sanitized();
        self.start.set(b.start);
        self.end.set(b.end);
        self.slices.store(b.slices, Ordering::Relaxed);
        self.snap.store(b.snap, Ordering::Relaxed);
        self.min_len_ms.set(b.min_len_ms);
        for (atoms, pad) in self.pads.iter().zip(b.pads) {
            atoms[0].set(pad.start);
            atoms[1].set(pad.end);
        }
    }

    /// Play `pad` once, as if a finger tapped it.
    pub fn audition(&self, pad: usize) {
        self.auditions[pad].fetch_add(1, Ordering::Relaxed);
    }

    /// How many times each pad has been auditioned; the slicer watches for changes.
    pub fn auditions(&self) -> [u32; PADS] {
        std::array::from_fn(|n| self.auditions[n].load(Ordering::Relaxed))
    }

    pub fn playhead(&self) -> Playhead {
        Playhead {
            len: self.len.get(),
            playing: self.playing.load(Ordering::Relaxed).then(|| Playing {
                pad: self.pad.load(Ordering::Relaxed) as usize,
                start: self.voice_start.get(),
                at: self.at.get(),
            }),
        }
    }

    pub fn set_playhead(&self, p: Playhead) {
        self.len.set(p.len);
        self.playing.store(p.playing.is_some(), Ordering::Relaxed);
        if let Some(v) = p.playing {
            self.pad.store(v.pad as u32, Ordering::Relaxed);
            self.voice_start.set(v.start);
            self.at.set(v.at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 1_000.0;

    fn free() -> Bounds {
        Bounds {
            snap: false,
            ..Bounds::default()
        }
    }

    #[test]
    fn the_default_pads_sit_on_the_first_grid_steps() {
        let b = Bounds::default();
        assert_eq!(
            b.pads[0],
            Slice {
                start: 0.0,
                end: 0.0625
            }
        );
        assert_eq!(
            b.pads[3],
            Slice {
                start: 0.1875,
                end: 0.25
            }
        );
        assert_eq!(b.pad_start(2, 1000.0), 125.0);
    }

    #[test]
    fn edges_snap_to_the_nearest_line_of_the_view() {
        let b = Bounds {
            start: 0.5,
            end: 1.0,
            slices: 4,
            ..Bounds::default()
        };
        assert_eq!(b.snap_point(0.68), 0.625);
        assert_eq!(b.snap_point(0.1), 0.125, "lines carry on outside the view");
        assert_eq!(free().snap_point(0.68), 0.68);
    }

    #[test]
    fn open_plays_the_whole_slice() {
        let b = free();
        // Pad 1 is 62.5 to 125 of 1000 samples, rounded to whole samples.
        assert_eq!(b.play_len(1, 1.0, SR, 1000.0), 62.0);
        assert_eq!(Bounds::default().play_len(1, 1.0, SR, 1000.0), 62.0);
    }

    #[test]
    fn pinched_snaps_to_halvings_of_the_slice() {
        let mut b = Bounds {
            min_len_ms: 10.0,
            ..Bounds::default()
        };
        b.pads[0] = Slice {
            start: 0.0,
            end: 0.16,
        };
        // 160 samples, halved down to the 10-sample floor.
        let lens: Vec<f32> = [1.0, 0.7, 0.4, 0.0]
            .iter()
            .map(|&len| b.play_len(0, len, SR, 1000.0))
            .collect();
        assert_eq!(lens, [160.0, 80.0, 40.0, 10.0]);
    }

    #[test]
    fn unsnapped_pinch_is_exponential() {
        let mut b = Bounds {
            min_len_ms: 16.0,
            ..free()
        };
        b.pads[0] = Slice {
            start: 0.0,
            end: 0.064,
        };
        assert_eq!(b.play_len(0, 0.5, SR, 1000.0), 32.0);
        assert_eq!(b.play_len(0, 0.0, SR, 1000.0), 16.0);
    }

    #[test]
    fn sanitizing_orders_the_view_and_the_slices() {
        let mut b = free();
        (b.start, b.end) = (0.8, 0.2);
        b.pads[2] = Slice {
            start: 0.5,
            end: 0.4,
        };
        let b = b.sanitized();
        assert_eq!((b.start, b.end), (0.2, 0.8));
        assert_eq!(
            b.pads[2],
            Slice {
                start: 0.4,
                end: 0.5
            }
        );
    }

    #[test]
    fn presets_round_trip_through_text() {
        let mut bounds = Bounds {
            start: 0.125,
            end: 0.5,
            slices: 8,
            snap: false,
            min_len_ms: 30.0,
            ..Bounds::default()
        };
        bounds.pads[1] = Slice {
            start: 0.3,
            end: 0.45,
        };
        let preset = Preset {
            bounds,
            beats: 2.0,
            follow_clock: false,
        };
        let back = Preset::guess(Bounds::default(), 2.0).from_text(&preset.to_text());
        assert_eq!(back, Ok(preset));
        assert!(
            Preset::guess(Bounds::default(), 2.0)
                .from_text("strat = 1")
                .is_err()
        );
    }

    #[test]
    fn an_old_file_without_pads_spreads_them_over_its_view() {
        let old = "start = 0.5\nend = 1.0\nslices = 8\nmax_len = 800\n";
        let p = Preset::guess(Bounds::default(), 2.0)
            .from_text(old)
            .unwrap();
        assert_eq!(
            p.bounds.pads[0],
            Slice {
                start: 0.5,
                end: 0.5625
            }
        );
    }

    #[test]
    fn the_guessed_tempo_is_plausible() {
        // Two bars of the amen at ~136 BPM.
        let amen = Preset::guess(Bounds::default(), 3.5555);
        assert_eq!(amen.beats, 8.0);
        assert!((amen.bpm(3.5555) - 135.0).abs() < 0.1);
    }

    #[test]
    fn shared_state_round_trips() {
        let shared = Shared::new(free());
        assert_eq!(shared.bounds(), free());
        let p = Playhead {
            len: 0.05,
            playing: Some(Playing {
                pad: 2,
                start: 0.2,
                at: 0.21,
            }),
        };
        shared.set_playhead(p);
        assert_eq!(shared.playhead(), p);

        shared.audition(3);
        shared.audition(3);
        assert_eq!(shared.auditions(), [0, 0, 0, 2]);
    }
}
