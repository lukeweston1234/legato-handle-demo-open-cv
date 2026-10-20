use legato::prelude::*;

use crate::gesture::{Gesture, PADS};

/// Output port indices, in the order of [`OUTPUTS`].
pub mod port {
    pub const GATE: usize = 0;
    pub const VEL: usize = 4;
    pub const HOLD: usize = 8;
    pub const POS: usize = 9;
    pub const PINCH: usize = 10;
    pub const HEIGHT: usize = 11;
    pub const ROLL: usize = 12;
    pub const DEPTH: usize = 13;
    pub const FIST: usize = 14;
}

const OUTPUTS: [&str; 15] = [
    "gate0", "gate1", "gate2", "gate3", "vel0", "vel1", "vel2", "vel3", "hold", "pos", "pinch",
    "height", "roll", "depth", "fist",
];

/// The continuous outputs, which are slewed rather than stepped.
const SMOOTHED: [usize; 5] = [
    port::POS,
    port::PINCH,
    port::HEIGHT,
    port::ROLL,
    port::DEPTH,
];

/// Outputs the latest [`Gesture`] pushed by the application as control signals.
///
/// Gestures arrive at camera rate (30–60 Hz), so the continuous outputs are
/// slewed per sample with a one-pole to hide the steps. Gates are stepped at
/// block boundaries, which at small block sizes is well under a millisecond.
pub struct Hands {
    rx: rtrb::Consumer<Gesture>,
    gesture: Gesture,
    smoothed: [f32; SMOOTHED.len()],
    smoothing_ms: f32,
    ports: Ports,
}

impl Hands {
    pub fn new(rx: rtrb::Consumer<Gesture>, smoothing_ms: f32) -> Self {
        let gesture = Gesture::default();
        Self {
            rx,
            smoothed: targets(&gesture),
            gesture,
            smoothing_ms,
            ports: PortBuilder::default().control_out_named(&OUTPUTS).build(),
        }
    }

    pub fn render(&mut self, sample_rate: f32, outputs: &mut [&mut [f32]]) {
        // A tap that opened and closed between two blocks still has to sound,
        // so a gate that rose at any point during the drain stays open for
        // this block even if the latest gesture has it closed again.
        let mut rose = [false; PADS];
        while let Ok(next) = self.rx.pop() {
            for (pad, rose) in rose.iter_mut().enumerate() {
                if next.gates[pad] && !self.gesture.gates[pad] {
                    *rose = true;
                }
            }
            self.gesture = next;
        }

        let g = &self.gesture;
        let bool_out = |b: bool| if b { 1.0 } else { 0.0 };
        for pad in 0..PADS {
            outputs[port::GATE + pad].fill(bool_out(g.gates[pad] || rose[pad]));
            outputs[port::VEL + pad].fill(g.velocity[pad]);
        }
        outputs[port::HOLD].fill(bool_out(g.hold));
        outputs[port::FIST].fill(bool_out(g.fist));

        let coeff = if self.smoothing_ms <= 0.0 {
            1.0
        } else {
            1.0 - (-1000.0 / (self.smoothing_ms * sample_rate)).exp()
        };
        for ((&out, y), target) in SMOOTHED
            .iter()
            .zip(&mut self.smoothed)
            .zip(targets(&self.gesture))
        {
            for sample in outputs[out].iter_mut() {
                *y += coeff * (target - *y);
                *sample = *y;
            }
        }
    }
}

fn targets(g: &Gesture) -> [f32; SMOOTHED.len()] {
    [g.pos, g.pinch, g.height, g.roll, g.depth]
}

impl Node for Hands {
    fn process(&mut self, ctx: &mut AudioContext, _: &Inputs, outputs: &mut [&mut [f32]]) {
        self.render(ctx.sample_rate_f32(), outputs);
    }
    fn ports(&self) -> &Ports {
        &self.ports
    }
}

impl NodeDefinition for Hands {
    const NAME: &'static str = "hands";
    const DESCRIPTION: &'static str =
        "Hand gestures pushed from the tracking thread, as control signals";
    const REQUIRED_PARAMS: &'static [&'static str] = &[];
    const OPTIONAL_PARAMS: &'static [&'static str] = &["smoothing", "capacity"];

    fn create(
        rb: &mut ResourceBuilderView,
        p: &DSLParams,
    ) -> Result<Box<dyn DynNode>, ValidationError> {
        let smoothing_ms = p.get_f32("smoothing")?.unwrap_or(20.0);
        let capacity = p.get_usize("capacity")?.unwrap_or(64);
        let (tx, rx) = rtrb::RingBuffer::new(capacity);
        rb.export_handle(tx)?;
        Ok(Box::new(Self::new(rx, smoothing_ms)))
    }
}

impl ExportsHandle for Hands {
    type Handle = rtrb::Producer<Gesture>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 64;
    const SR: f32 = 48_000.0;

    fn render(node: &mut Hands) -> Vec<Vec<f32>> {
        let mut bufs = vec![vec![f32::NAN; BLOCK]; OUTPUTS.len()];
        let mut outs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
        node.render(SR, &mut outs);
        bufs
    }

    fn pair(smoothing_ms: f32) -> (rtrb::Producer<Gesture>, Hands) {
        let (tx, rx) = rtrb::RingBuffer::new(16);
        (tx, Hands::new(rx, smoothing_ms))
    }

    #[test]
    fn gates_follow_the_latest_gesture() {
        let (mut tx, mut node) = pair(0.0);
        let mut g = Gesture::default();
        g.gates[2] = true;
        g.velocity[2] = 0.7;
        g.hold = true;
        tx.push(g).unwrap();

        let out = render(&mut node);
        assert!(out[port::GATE + 2].iter().all(|&x| x == 1.0));
        assert!(out[port::GATE].iter().all(|&x| x == 0.0));
        assert!(out[port::VEL + 2].iter().all(|&x| x == 0.7));
        assert!(out[port::HOLD].iter().all(|&x| x == 1.0));
    }

    #[test]
    fn a_tap_between_blocks_lasts_one_block() {
        let (mut tx, mut node) = pair(0.0);
        let mut down = Gesture::default();
        down.gates[0] = true;
        tx.push(down).unwrap();
        tx.push(Gesture::default()).unwrap();

        assert!(render(&mut node)[port::GATE].iter().all(|&x| x == 1.0));
        assert!(render(&mut node)[port::GATE].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn continuous_controls_slew_towards_the_target() {
        let (mut tx, mut node) = pair(20.0);
        tx.push(Gesture {
            pos: 1.0,
            ..Gesture::default()
        })
        .unwrap();

        let first = render(&mut node)[port::POS].clone();
        assert!(first.windows(2).all(|w| w[1] > w[0]), "rises monotonically");
        assert!(first[BLOCK - 1] < 0.1, "no jump: {}", first[BLOCK - 1]);

        // ~5 time constants later it has arrived.
        let mut last = 0.0;
        for _ in 0..(5.0 * 0.02 * SR / BLOCK as f32) as usize {
            last = render(&mut node)[port::POS][BLOCK - 1];
        }
        assert!(last > 0.99, "{last}");
    }

    #[test]
    fn no_gesture_outputs_neutral_defaults() {
        let (_tx, mut node) = pair(20.0);
        let out = render(&mut node);
        assert!(out[port::HEIGHT].iter().all(|&x| x == 0.5));
        assert!(out[port::GATE].iter().all(|&x| x == 0.0));
    }
}
