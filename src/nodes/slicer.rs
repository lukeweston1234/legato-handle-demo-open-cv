use std::sync::Arc;

use legato::{
    math::cubic_hermite,
    prelude::*,
    resources::{ExternalBufferKey, buffer::ExternalBuffer},
};

use crate::{
    bounds::{Bounds, Playhead, Playing, Shared},
    gesture::PADS,
};

/// Input port indices, in the order of [`INPUTS`].
pub mod port {
    pub const GATE: usize = 0;
    pub const VEL: usize = 4;
    pub const HOLD: usize = 8;
    pub const LEN: usize = 9;
    pub const PITCH: usize = 10;
    pub const RATE: usize = 11;
    pub const SYNC: usize = 12;
}

const INPUTS: [&str; 13] = [
    "gate0", "gate1", "gate2", "gate3", "vel0", "vel1", "vel2", "vel3", "hold", "len", "pitch",
    "rate", "sync",
];

/// Four pads, each playing its own slice of one sample, one at a time.
///
/// Where the slices are is set from outside through the handle (by the
/// viewer); the inputs only choose which one plays and how. The playing
/// slice latches its start when it fires, but follows `len`, `pitch` and
/// `rate` live, so pinching while holding a loop rolls it.
///
/// It is monophonic: firing a pad cuts off whatever was playing. If several
/// pads fire in the same grid step, the highest velocity wins, so a finger
/// dragged along by its neighbour doesn't steal the slice.
///
/// - `gate0..3` fire a pad on the rising edge and keep it looping while high.
///   A key held in the viewer gates its pad the same way.
///   Once released, it finishes the cycle it is on, so a quick tap still
///   plays the whole slice.
/// - `vel0..3` scale the pad, latched when it fires. Unconnected means full.
/// - `hold` keeps the slice looping after its gate closes.
/// - `len` (0..1) is how much of the slice plays: all of it when open, down
///   to `min_len` ms, in halvings when snapped. See [`Bounds::play_len`].
/// - `pitch` is in semitones; `rate` multiplies speed, and negative plays backwards.
/// - `sync` takes a `clock` phase. When connected, pads only fire when it
///   crosses one of `steps` steps, so taps land on the grid.
///
/// Each loop cycle has a short fade at both ends so the seam doesn't click,
/// and a new slice fades the old playhead out rather than jumping.
///
/// It exports an `Arc<Shared>` handle: the viewer writes [`Bounds`] into it,
/// read at the start of every block, can audition pads through it, and the
/// slicer writes back its [`Playhead`] at the end of every block.
pub struct Slicer {
    key: ExternalBufferKey,
    voice: Voice,
    /// The pad the voice is playing.
    pad: usize,
    gates: [bool; PADS],
    /// The audition counts last seen, to notice new ones.
    heard: [u32; PADS],
    /// (pad, velocity) of the press waiting for the next grid step.
    pending: Option<(usize, f32)>,
    last_step: Option<i64>,
    steps: f32,
    shared: Arc<Shared>,
    fade_ms: f32,
    gain: f32,
    ports: Ports,
}

#[derive(Debug, Clone, Copy, Default)]
struct Voice {
    playing: bool,
    /// Where the slice begins in the buffer, latched when it fired.
    start: usize,
    /// Offset into the slice, in samples.
    phase: f32,
    vel: f32,
    ghost: Ghost,
}

/// The previous playhead after a new slice fires, fading out.
#[derive(Debug, Clone, Copy, Default)]
struct Ghost {
    start: usize,
    phase: f32,
    speed: f32,
    amp: f32,
    left: f32,
}

/// One read from the buffer: `amp * sample[start + phase]`.
#[derive(Debug, Clone, Copy, Default)]
struct Tap {
    start: usize,
    phase: f32,
    amp: f32,
}

impl Voice {
    fn fire(&mut self, start: usize, vel: f32, speed: f32, len: f32, fade: f32) {
        if self.playing {
            self.ghost = Ghost {
                start: self.start,
                phase: self.phase,
                speed,
                amp: self.vel * window(self.phase, len, fade),
                left: fade,
            };
        }
        self.playing = true;
        self.start = start;
        self.vel = vel;
        self.phase = if speed < 0.0 { len - 1.0 } else { 0.0 };
    }

    /// Advance one sample, returning the main and ghost reads.
    fn next(&mut self, len: f32, speed: f32, fade: f32, looping: bool) -> [Tap; 2] {
        let mut main = Tap::default();
        if self.playing {
            main = Tap {
                start: self.start,
                phase: self.phase,
                amp: self.vel * window(self.phase, len, fade),
            };
            self.phase += speed;
            if !(0.0..len).contains(&self.phase) {
                if looping {
                    self.phase = self.phase.rem_euclid(len);
                } else {
                    self.playing = false;
                }
            }
        }

        let mut ghost = Tap::default();
        let g = &mut self.ghost;
        if g.left > 0.0 {
            ghost = Tap {
                start: g.start,
                phase: g.phase,
                amp: g.amp * g.left / fade,
            };
            g.phase += g.speed;
            g.left -= 1.0;
        }
        [main, ghost]
    }
}

/// A trapezoid over one loop cycle: ramps in and out over `fade` samples, or
/// a triangle when the slice is shorter than two fades. No fade is no window.
#[inline(always)]
fn window(phase: f32, len: f32, fade: f32) -> f32 {
    if fade < 1.0 {
        return 1.0;
    }
    let fade = fade.min(len * 0.5).max(1.0);
    (phase / fade).min((len - phase) / fade).clamp(0.0, 1.0)
}

/// Cubic read at `start + phase`, wrapping around the buffer.
#[inline(always)]
fn read(src: &[f32], tap: Tap) -> f32 {
    let n = src.len() as isize;
    let floor = tap.phase.floor();
    let index = tap.start as isize + floor as isize;
    let at = |offset: isize| src[(index + offset).rem_euclid(n) as usize];
    cubic_hermite(at(-1), at(0), at(1), at(2), tap.phase - floor)
}

impl Slicer {
    pub fn new(key: ExternalBufferKey, chans: usize, shared: Arc<Shared>) -> Self {
        Self {
            key,
            voice: Voice::default(),
            pad: 0,
            gates: [false; PADS],
            heard: [0; PADS],
            pending: None,
            last_step: None,
            steps: 16.0,
            shared,
            fade_ms: 4.0,
            gain: 0.7,
            ports: PortBuilder::default()
                .control_in_named(&INPUTS)
                .audio_out(chans)
                .build(),
        }
    }

    /// Renders one block from `buffer`, which must be non-empty.
    pub fn render(
        &mut self,
        buffer: &ExternalBuffer,
        inputs: &Inputs,
        outputs: &mut [&mut [f32]],
        sample_rate: f32,
    ) {
        let total = buffer.len_samples();
        let block_size = outputs.first().map_or(0, |c| c.len());
        let fade = self.fade_ms * sample_rate * 0.001;
        let bounds = self.shared.bounds();
        let total_f = total as f32;
        let mut len = 0.0;

        // An audition is a tap at full velocity, quantized like any other.
        let auditions = self.shared.auditions();
        let keys: [bool; PADS] = std::array::from_fn(|pad| self.shared.held(pad));
        for (pad, (now, heard)) in auditions.iter().zip(&mut self.heard).enumerate() {
            if std::mem::replace(heard, *now) != *now {
                self.pending = Some((pad, 1.0));
            }
        }

        for c in outputs.iter_mut() {
            c.fill(0.0);
        }

        for i in 0..block_size {
            let at = |port: usize, default: f32| inputs[port].map_or(default, |x| x[i]);

            let hold = at(port::HOLD, 0.0) >= 0.5;
            let len_in = at(port::LEN, 1.0);
            let speed = at(port::RATE, 1.0) * (at(port::PITCH, 0.0) / 12.0).exp2();

            let on_grid = match inputs[port::SYNC] {
                None => true,
                Some(sync) => {
                    let step = (sync[i] * self.steps).floor() as i64;
                    self.last_step.replace(step) != Some(step)
                }
            };

            for (pad, was) in self.gates.iter_mut().enumerate() {
                let finger = at(port::GATE + pad, 0.0) >= 0.5;
                let gate = finger || keys[pad];
                if gate && !*was {
                    // A key has no velocity of its own; play it full.
                    let vel = if finger {
                        at(port::VEL + pad, 1.0).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    if self.pending.is_none_or(|(_, waiting)| vel >= waiting) {
                        self.pending = Some((pad, vel));
                    }
                }
                *was = gate;
            }

            if on_grid && let Some((pad, vel)) = self.pending.take() {
                let start = bounds.pad_start(pad, total_f) as usize;
                len = bounds.play_len(pad, len_in, sample_rate, total_f);
                self.pad = pad;
                self.voice.fire(start.min(total - 1), vel, speed, len, fade);
            }
            len = bounds.play_len(self.pad, len_in, sample_rate, total_f);

            let looping = self.gates[self.pad] || hold;
            let taps = self.voice.next(len, speed, fade, looping);
            for (c, out) in outputs.iter_mut().enumerate() {
                let src = buffer.channel(c % buffer.num_channels);
                for tap in taps.iter().filter(|t| t.amp > 0.0) {
                    out[i] += self.gain * tap.amp * read(src, *tap);
                }
            }
        }

        let total = total_f;
        let v = &self.voice;
        self.shared.set_playhead(Playhead {
            len: len / total,
            playing: v.playing.then(|| Playing {
                pad: self.pad,
                start: v.start as f32 / total,
                at: (v.start as f32 + v.phase).rem_euclid(total) / total,
            }),
        });
    }
}

impl Node for Slicer {
    fn process(&mut self, ctx: &mut AudioContext, inputs: &Inputs, outputs: &mut [&mut [f32]]) {
        let sample_rate = ctx.sample_rate_f32();
        match ctx.get_resources().get_external_buffer(self.key) {
            Some(buffer) if buffer.num_channels > 0 && buffer.len_samples() > 0 => {
                self.render(buffer, inputs, outputs, sample_rate)
            }
            _ => outputs.iter_mut().for_each(|c| c.fill(0.0)),
        }
    }
    fn ports(&self) -> &Ports {
        &self.ports
    }
}

impl NodeDefinition for Slicer {
    const NAME: &'static str = "slicer";
    const DESCRIPTION: &'static str =
        "Four pads each playing its own slice of a sample, one at a time, optionally on a grid";
    const REQUIRED_PARAMS: &'static [&'static str] = &["sampler_name"];
    const OPTIONAL_PARAMS: &'static [&'static str] =
        &["chans", "steps", "min_len", "slices", "fade", "gain"];

    fn create(
        rb: &mut ResourceBuilderView,
        p: &DSLParams,
    ) -> Result<Box<dyn DynNode>, ValidationError> {
        let name = p.require_str("sampler_name")?;
        let chans = p.get_usize("chans")?.unwrap_or(2);
        let defaults = Bounds::default();
        let shared = Arc::new(Shared::new(
            Bounds {
                min_len_ms: p.get_f32("min_len")?.unwrap_or(defaults.min_len_ms),
                slices: p.get_usize("slices")?.map_or(defaults.slices, |n| n as u32),
                ..defaults
            }
            .spread(),
        ));
        rb.export_handle(shared.clone())?;
        let mut node = Self::new(rb.add_external_buffer_key(&name), chans, shared);

        node.steps = p.get_f32("steps")?.unwrap_or(node.steps).max(1.0);
        node.fade_ms = p.get_f32("fade")?.unwrap_or(node.fade_ms).max(0.0);
        node.gain = p.get_f32("gain")?.unwrap_or(node.gain);
        Ok(Box::new(node))
    }
}

impl ExportsHandle for Slicer {
    type Handle = Arc<Shared>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::Slice;

    const BLOCK: usize = 64;
    /// 1 kHz makes ms and samples line up: 1 sample per ms.
    const SR: f32 = 1_000.0;

    fn ramp(len: usize) -> ExternalBuffer {
        ExternalBuffer {
            data: (0..len).map(|x| x as f32).collect(),
            num_channels: 1,
        }
    }

    fn sine(len: usize) -> ExternalBuffer {
        ExternalBuffer {
            data: (0..len).map(|x| (x as f32 * 0.05).sin()).collect(),
            num_channels: 1,
        }
    }

    /// Pad `n` plays 16 samples from `100 + 16n`, with no fade, no snapping
    /// and unity gain so the output is the raw buffer value.
    fn slicer() -> Slicer {
        let mut b = Bounds {
            snap: false,
            min_len_ms: 4.0,
            ..Bounds::default()
        };
        for (n, pad) in b.pads.iter_mut().enumerate() {
            let start = (100.0 + 16.0 * n as f32) / 1000.0;
            *pad = Slice {
                start,
                end: start + 0.016,
            };
        }
        let mut s = Slicer::new(ExternalBufferKey::default(), 1, Arc::new(Shared::new(b)));
        s.fade_ms = 0.0;
        s.gain = 1.0;
        s
    }

    fn set_bounds(node: &Slicer, edit: impl FnOnce(&mut Bounds)) {
        let mut b = node.shared.bounds();
        edit(&mut b);
        node.shared.set_bounds(b);
    }

    /// Render with `set` filling in the given input ports for every sample.
    fn render(node: &mut Slicer, buffer: &ExternalBuffer, set: &[(usize, &[f32])]) -> Vec<f32> {
        let mut inputs: [Option<&[f32]>; INPUTS.len()] = [None; INPUTS.len()];
        for &(port, data) in set {
            inputs[port] = Some(data);
        }
        let mut out = vec![f32::NAN; BLOCK];
        node.render(buffer, &inputs, &mut [&mut out], SR);
        out
    }

    fn pulse(from: usize, to: usize) -> Vec<f32> {
        (0..BLOCK)
            .map(|i| ((from..to).contains(&i)) as u8 as f32)
            .collect()
    }

    #[test]
    fn silent_until_a_pad_fires() {
        let out = render(&mut slicer(), &ramp(1000), &[]);
        assert!(out.iter().all(|&y| y == 0.0));
    }

    #[test]
    fn each_pad_plays_its_own_slice() {
        let buffer = ramp(1000);
        for pad in 0..PADS {
            let out = render(
                &mut slicer(),
                &buffer,
                &[(port::GATE + pad, &pulse(0, BLOCK))],
            );
            let start = 100.0 + 16.0 * pad as f32;
            assert_eq!(out[0], start, "pad {pad}");
            assert_eq!(out[5], start + 5.0, "pad {pad}");
        }
    }

    #[test]
    fn slices_can_go_anywhere() {
        let node = &mut slicer();
        set_bounds(node, |b| {
            b.pads[2] = Slice {
                start: 0.9,
                end: 0.93,
            }
        });
        let out = render(node, &ramp(1000), &[(port::GATE + 2, &pulse(0, BLOCK))]);
        assert_eq!(out[0], 900.0);
        assert_eq!(out[29], 929.0);
        assert_eq!(out[30], 900.0, "loops the 30-sample slice");
    }

    #[test]
    fn loops_while_held_and_finishes_the_cycle_when_released() {
        let buffer = ramp(1000);
        let held = render(&mut slicer(), &buffer, &[(port::GATE, &pulse(0, BLOCK))]);
        assert_eq!(held[16], 100.0, "wrapped back to the slice start");
        assert_eq!(held[40], 108.0);

        let tapped = render(&mut slicer(), &buffer, &[(port::GATE, &pulse(0, 2))]);
        assert_eq!(tapped[15], 115.0, "a short tap plays the whole slice");
        assert!(tapped[16..].iter().all(|&y| y == 0.0));
    }

    #[test]
    fn hold_keeps_a_released_pad_looping() {
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[(port::GATE, &pulse(0, 2)), (port::HOLD, &[1.0; BLOCK])],
        );
        assert_eq!(out[40], 108.0);
    }

    #[test]
    fn velocity_scales_the_pad() {
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[(port::GATE, &pulse(0, BLOCK)), (port::VEL, &[0.5; BLOCK])],
        );
        assert_eq!(out[10], 55.0);
    }

    #[test]
    fn sync_delays_a_press_to_the_next_step() {
        // A phase ramping 0..1 across the block.
        let phase: Vec<f32> = (0..BLOCK).map(|i| i as f32 / BLOCK as f32).collect();
        let play = |steps: f32| {
            let mut node = slicer();
            node.steps = steps;
            render(
                &mut node,
                &ramp(1000),
                &[(port::GATE, &pulse(5, 7)), (port::SYNC, &phase)],
            )
        };

        // A step every sample: the press at 5 fires at once.
        let fine = play(BLOCK as f32);
        assert_eq!(fine[6], 101.0);

        // A step every 16 samples: it waits until 16, then plays the slice.
        let coarse = play(4.0);
        assert!(coarse[..16].iter().all(|&y| y == 0.0));
        assert_eq!(coarse[16], 100.0);
        assert_eq!(coarse[31], 115.0);
    }

    #[test]
    fn a_new_pad_cuts_off_the_old_one() {
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[
                (port::GATE, &pulse(0, BLOCK)),
                (port::GATE + 2, &pulse(8, BLOCK)),
            ],
        );
        assert_eq!(out[7], 107.0, "pad 0 until pad 2 fires");
        // Pad 2's slice starts at 132, and pad 0 is gone even though it is still held.
        assert_eq!(out[8], 132.0);
        assert_eq!(out[30], 132.0 + 6.0);
    }

    #[test]
    fn the_fastest_finger_in_a_step_wins() {
        let phase: Vec<f32> = (0..BLOCK).map(|i| i as f32 / BLOCK as f32).collect();
        let mut node = slicer();
        node.steps = 4.0;
        // Ring pressed hard, pinky dragged along softly a little later.
        let out = render(
            &mut node,
            &ramp(1000),
            &[
                (port::GATE + 2, &pulse(3, BLOCK)),
                (port::VEL + 2, &[1.0; BLOCK]),
                (port::GATE + 3, &pulse(5, BLOCK)),
                (port::VEL + 3, &[0.3; BLOCK]),
                (port::SYNC, &phase),
            ],
        );
        assert_eq!(out[16], 132.0, "ring's slice, at full velocity");
    }

    #[test]
    fn negative_rate_plays_the_slice_backwards() {
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[(port::GATE, &pulse(0, BLOCK)), (port::RATE, &[-1.0; BLOCK])],
        );
        assert_eq!(out[0], 115.0);
        assert_eq!(out[15], 100.0);
        assert_eq!(out[16], 115.0);
    }

    #[test]
    fn pitch_is_in_semitones() {
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[
                (port::GATE, &pulse(0, BLOCK)),
                (port::PITCH, &[12.0; BLOCK]),
            ],
        );
        assert_eq!(out[4], 108.0);
    }

    #[test]
    fn pinching_shortens_the_slice_from_its_start() {
        // Unsnapped, 4 to 16 samples: halfway is 8.
        let out = render(
            &mut slicer(),
            &ramp(1000),
            &[(port::GATE, &pulse(0, BLOCK)), (port::LEN, &[0.5; BLOCK])],
        );
        assert_eq!(out[7], 107.0);
        assert_eq!(out[8], 100.0);
    }

    #[test]
    fn retrigger_and_loop_seams_do_not_click() {
        let buffer = sine(4000);
        let mut node = slicer();
        node.fade_ms = 4.0;
        set_bounds(&node, |b| {
            for (n, pad) in b.pads.iter_mut().enumerate() {
                // Unrelated 40-sample slices, so each retrigger jumps.
                let start = [0.05, 0.43, 0.21, 0.77][n];
                *pad = Slice {
                    start,
                    end: start + 0.01,
                };
            }
        });

        // Hammer a different pad every 8 samples, mid-loop.
        let gates: Vec<Vec<f32>> = (0..PADS)
            .map(|pad| {
                (0..BLOCK)
                    .map(|i| ((i / 8) % PADS == pad && i % 8 < 4) as u8 as f32)
                    .collect()
            })
            .collect();
        let set: Vec<(usize, &[f32])> = (0..PADS)
            .map(|p| (port::GATE + p, gates[p].as_slice()))
            .collect();
        let mut all = Vec::new();
        for _ in 0..4 {
            all.extend(render(&mut node, &buffer, &set));
        }
        // The sine moves at most 0.05 per sample; fades add at most 2/4 per sample.
        let worst = all
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(worst < 0.6, "jump of {worst}");
        assert!(all.iter().any(|&y| y.abs() > 0.3), "and it is not silent");
    }

    #[test]
    fn auditioning_plays_a_pad_once() {
        let node = &mut slicer();
        let buffer = ramp(1000);
        node.shared.audition(1);
        let out = render(node, &buffer, &[]);
        assert_eq!(out[0], 116.0);
        assert!(out[16..].iter().all(|&y| y == 0.0), "a tap, not a loop");
        assert!(
            render(node, &buffer, &[]).iter().all(|&y| y == 0.0),
            "only once"
        );
    }

    #[test]
    fn a_held_key_loops_like_a_finger() {
        let node = &mut slicer();
        let buffer = ramp(1000);
        node.shared.set_held(0b0010);
        let out = render(node, &buffer, &[]);
        assert_eq!(out[0], 116.0);
        assert_eq!(out[40], 116.0 + 8.0, "still looping");

        // Pinching while held shortens the loop: 4 to 16 samples, halfway is 8.
        let out = render(node, &buffer, &[(port::LEN, &[0.5; BLOCK])]);
        assert!(out.iter().all(|&y| (116.0..124.0).contains(&y)));

        node.shared.set_held(0);
        render(node, &buffer, &[]);
        assert!(
            render(node, &buffer, &[]).iter().all(|&y| y == 0.0),
            "stops once let go"
        );
    }

    #[test]
    fn edits_reach_a_playing_slice_on_the_next_fire() {
        let node = &mut slicer();
        let buffer = ramp(1000);
        render(node, &buffer, &[(port::GATE, &pulse(0, 2))]);
        set_bounds(node, |b| {
            b.pads[0] = Slice {
                start: 0.5,
                end: 0.516,
            }
        });
        let out = render(node, &buffer, &[(port::GATE, &pulse(0, 2))]);
        assert_eq!(out[0], 500.0);
    }

    #[test]
    fn the_playhead_is_reported_back() {
        let node = &mut slicer();
        let buffer = ramp(1000);
        render(node, &buffer, &[]);
        assert_eq!(node.shared.playhead().playing, None);

        render(node, &buffer, &[(port::GATE + 1, &pulse(0, BLOCK))]);
        let head = node.shared.playhead();
        let playing = head.playing.expect("pad 1 is sounding");
        assert_eq!((playing.pad, playing.start), (1, 0.116));
        assert_eq!(head.len, 0.016);
        // 64 samples into a 16-sample loop is back at its start.
        assert_eq!(playing.at, 0.116);
    }
}
