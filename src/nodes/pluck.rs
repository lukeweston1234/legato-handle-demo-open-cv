use legato::prelude::*;

/// Input port indices, in the order they are built.
pub mod port {
    pub const GATE: usize = 0;
    pub const ATTACK: usize = 1;
    pub const DECAY: usize = 2;
    pub const AUDIO: usize = 3;
}

/// The level a decay counts as finished at, -60 dB.
const FLOOR: f32 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Attack,
    Decay,
}

/// An attack–decay envelope applied to its input, with both times patchable.
///
/// Legato's `adsr` takes its times as fixed params; this one reads `attack`
/// and `decay` (ms) as control inputs, so a hand can shape the notes while
/// they play. A rising `gate` restarts the attack from the current level, so
/// fast retriggers swell rather than click. The attack is linear; the decay
/// is exponential, reaching -60 dB after `decay` ms, and ignores the gate's
/// length, like a plucked string.
pub struct Pluck {
    stage: Stage,
    level: f32,
    gate: bool,
    attack_ms: f32,
    decay_ms: f32,
    ports: Ports,
}

impl Pluck {
    pub fn new(attack_ms: f32, decay_ms: f32) -> Self {
        Self {
            stage: Stage::Idle,
            level: 0.0,
            gate: false,
            attack_ms,
            decay_ms,
            ports: PortBuilder::default()
                .control_in_named(&["gate", "attack", "decay"])
                .audio_in_named(&["in"])
                .audio_out(1)
                .build(),
        }
    }

    pub fn render(&mut self, sample_rate: f32, inputs: &Inputs, out: &mut [f32]) {
        // Times change at camera rate, so once a block is plenty, and keeps
        // the exp out of the per-sample loop.
        let at_start = |port: usize, fallback: f32| inputs[port].map_or(fallback, |x| x[0]);
        let ms_to_samples = sample_rate * 0.001;
        let attack = (at_start(port::ATTACK, self.attack_ms) * ms_to_samples).max(1.0);
        let decay = (at_start(port::DECAY, self.decay_ms) * ms_to_samples).max(1.0);
        let rise = 1.0 / attack;
        let fall = FLOOR.powf(1.0 / decay);

        for (i, y) in out.iter_mut().enumerate() {
            let gate = inputs[port::GATE].is_some_and(|g| g[i] >= 0.5);
            if gate && !self.gate {
                self.stage = Stage::Attack;
            }
            self.gate = gate;

            match self.stage {
                Stage::Idle => {}
                Stage::Attack => {
                    self.level += rise;
                    if self.level >= 1.0 {
                        self.level = 1.0;
                        self.stage = Stage::Decay;
                    }
                }
                Stage::Decay => {
                    self.level *= fall;
                    if self.level < FLOOR {
                        self.level = 0.0;
                        self.stage = Stage::Idle;
                    }
                }
            }
            *y = inputs[port::AUDIO].map_or(0.0, |x| x[i]) * self.level;
        }
    }
}

impl Node for Pluck {
    fn process(&mut self, ctx: &mut AudioContext, inputs: &Inputs, outputs: &mut [&mut [f32]]) {
        self.render(ctx.sample_rate_f32(), inputs, outputs[0]);
    }
    fn ports(&self) -> &Ports {
        &self.ports
    }
}

impl NodeDefinition for Pluck {
    const NAME: &'static str = "pluck";
    const DESCRIPTION: &'static str =
        "Attack-decay envelope on its input, with attack and decay as control inputs in ms";
    const REQUIRED_PARAMS: &'static [&'static str] = &[];
    const OPTIONAL_PARAMS: &'static [&'static str] = &["attack", "decay"];

    fn create(
        _: &mut ResourceBuilderView,
        p: &DSLParams,
    ) -> Result<Box<dyn DynNode>, ValidationError> {
        Ok(Box::new(Self::new(
            p.get_f32("attack")?.unwrap_or(2.0).max(0.0),
            p.get_f32("decay")?.unwrap_or(200.0).max(0.0),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 480; // 10 ms

    /// Renders `blocks` of a constant 1.0 input and returns the envelope.
    fn run(p: &mut Pluck, gate: &[bool], attack: f32, decay: f32) -> Vec<f32> {
        let mut env = Vec::new();
        for &g in gate {
            let gate_buf = vec![if g { 1.0 } else { 0.0 }; BLOCK];
            let (a, d, one) = (vec![attack; BLOCK], vec![decay; BLOCK], vec![1.0; BLOCK]);
            let inputs = [
                Some(&gate_buf[..]),
                Some(&a[..]),
                Some(&d[..]),
                Some(&one[..]),
            ];
            let mut out = vec![0.0; BLOCK];
            p.render(SR, &inputs, &mut out);
            env.extend(out);
        }
        env
    }

    #[test]
    fn rises_over_attack_then_decays_over_decay() {
        let mut p = Pluck::new(0.0, 0.0);
        // Gate for 10 ms, then let go: the decay doesn't care.
        let mut gate = vec![false; 40];
        gate[0] = true;
        let env = run(&mut p, &gate, 20.0, 100.0);
        let ms = |t: f32| env[(t * SR / 1000.0) as usize];

        assert!((ms(10.0) - 0.5).abs() < 0.01, "halfway up: {}", ms(10.0));
        assert!(ms(20.1) > 0.99, "peak at the attack time");
        // 90 ms into a 100 ms decay is -54 dB.
        let expected = FLOOR.powf(0.9);
        assert!((ms(110.0) - expected).abs() < 0.0002, "{}", ms(110.0));
        assert_eq!(ms(200.0), 0.0, "and then silent");
    }

    #[test]
    fn retrigger_starts_from_the_current_level() {
        let mut p = Pluck::new(0.0, 0.0);
        let env = run(&mut p, &[true, false, false, false, true], 2.0, 500.0);
        let before = env[4 * BLOCK - 1];
        let after = env[4 * BLOCK];
        assert!(before > 0.1);
        assert!(
            after > before && after - before < 0.02,
            "no jump: {before} -> {after}"
        );
    }

    #[test]
    fn times_follow_their_inputs() {
        let short = run(&mut Pluck::new(0.0, 0.0), &[true; 10], 1.0, 30.0);
        let long = run(&mut Pluck::new(0.0, 0.0), &[true; 10], 1.0, 600.0);
        assert!(long[BLOCK * 5] > 10.0 * short[BLOCK * 5]);
    }
}
