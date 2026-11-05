use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering::Relaxed},
};

use legato::prelude::*;

use crate::gesture::PADS;

/// The most voices one harmony node quantizes.
pub const MAX_VOICES: usize = 8;

const GATES: [&str; PADS] = ["gate0", "gate1", "gate2", "gate3"];
const PITCHES: [&str; MAX_VOICES] = [
    "pitch0", "pitch1", "pitch2", "pitch3", "pitch4", "pitch5", "pitch6", "pitch7",
];
const FREQS: [&str; MAX_VOICES] = [
    "freq0", "freq1", "freq2", "freq3", "freq4", "freq5", "freq6", "freq7",
];

/// Input port indices: the gates, `hold`, then one pitch per voice.
pub mod port {
    pub const GATE: usize = 0;
    pub const HOLD: usize = 4;
    pub const PITCH: usize = 5;
}

/// A set of pitch classes, bit `n` for semitone `n` above C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PitchSet(pub u16);

impl PitchSet {
    pub fn from_semitones(notes: impl IntoIterator<Item = i32>) -> Self {
        Self(
            notes
                .into_iter()
                .fold(0, |bits, n| bits | 1 << n.rem_euclid(12)),
        )
    }

    pub fn contains(self, pc: i32) -> bool {
        self.0 & 1 << pc.rem_euclid(12) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The member nearest to the MIDI note `pitch`, ties going down.
    pub fn snap(self, pitch: f32) -> Option<i32> {
        if self.is_empty() {
            return None;
        }
        let centre = pitch.round() as i32;
        (0..=6).find_map(|d| {
            [centre - d, centre + d]
                .into_iter()
                .filter(|&n| self.contains(n))
                .min_by(|a, b| {
                    (*a as f32 - pitch)
                        .abs()
                        .total_cmp(&(*b as f32 - pitch).abs())
                })
        })
    }
}

/// One finger's chord: its pitch classes, with the first as its root.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chord {
    pub root: i32,
    pub tones: PitchSet,
}

impl Chord {
    pub fn new(notes: &[i32]) -> Self {
        Self {
            root: notes.first().copied().unwrap_or(0).rem_euclid(12),
            tones: PitchSet::from_semitones(notes.iter().copied()),
        }
    }

    /// A lead-sheet name in the key of MIDI note `key`, e.g. "Am9", or the
    /// root and a "?" when it isn't a triad, seventh or ninth.
    pub fn name(&self, key: i32) -> String {
        const NOTES: [&str; 12] = [
            "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
        ];
        let rel = |n: i32| self.tones.contains(self.root + n);
        let quality = match (rel(3), rel(4), rel(7), rel(10), rel(11), rel(2)) {
            (false, true, true, false, false, false) => "",
            (true, false, true, false, false, false) => "m",
            (false, true, true, true, false, false) => "7",
            (true, false, true, true, false, false) => "m7",
            (false, true, true, false, true, false) => "maj7",
            (false, true, true, false, false, true) => "add9",
            (true, false, true, false, false, true) => "m(add9)",
            (false, true, true, true, false, true) => "9",
            (true, false, true, true, false, true) => "m9",
            (false, true, true, false, true, true) => "maj9",
            _ => "?",
        };
        format!(
            "{}{quality}",
            NOTES[(key + self.root).rem_euclid(12) as usize]
        )
    }
}

/// What the harmony node is playing, for the tracker's overlay and tests.
///
/// The audio thread writes it at the end of every block; anyone may read it.
#[derive(Debug)]
pub struct Shared {
    /// Each finger's chord name, fixed when the patch is built.
    names: [String; PADS],
    /// Bits 0..4 are the fingers sounding (held or latched), bit 4 is hold.
    fingers: AtomicU32,
    /// MIDI note per voice.
    notes: [AtomicU32; MAX_VOICES],
    voices: AtomicU32,
}

impl Shared {
    pub fn new(key: i32, chords: &[Chord; PADS]) -> Self {
        Self {
            names: chords.each_ref().map(|c| c.name(key)),
            fingers: AtomicU32::default(),
            notes: Default::default(),
            voices: AtomicU32::default(),
        }
    }
    pub fn names(&self) -> &[String; PADS] {
        &self.names
    }
    pub fn voices(&self) -> usize {
        self.voices.load(Relaxed) as usize
    }
    pub fn fingers(&self) -> [bool; PADS] {
        let bits = self.fingers.load(Relaxed);
        std::array::from_fn(|f| bits & 1 << f != 0)
    }
    pub fn holding(&self) -> bool {
        self.fingers.load(Relaxed) & 1 << PADS != 0
    }
    pub fn note(&self, voice: usize) -> u8 {
        self.notes[voice].load(Relaxed) as u8
    }
}

/// Snaps generative pitches onto the chord the playing hand is holding.
///
/// Each curled finger adds its chord (`finger0..3`, pitch classes with the
/// root first); several fingers sound their union. With no finger curled the
/// voices fall back to `scale`, so the patch keeps playing on its own. A
/// tucked thumb (`hold`) latches the fingers that were curled when it went in.
///
/// - `pitch0..N` take MIDI note numbers (e.g. `random.stepped`) and come out
///   as Hz on `freq0..N`, re-snapped every sample, so sustained notes move
///   with a chord change.
/// - The first `roots` voices snap to chord roots only, for a bass line.
///
/// It exports an `Arc<Shared>` so the overlay can show what it is playing.
pub struct Harmony {
    root: i32,
    scale: PitchSet,
    scale_roots: PitchSet,
    chords: [Chord; PADS],
    roots: usize,
    voices: usize,
    latched: [bool; PADS],
    shared: Arc<Shared>,
    ports: Ports,
}

impl Harmony {
    pub fn new(
        voices: usize,
        root: i32,
        scale: &[i32],
        chords: [Chord; PADS],
        roots: usize,
        shared: Arc<Shared>,
    ) -> Self {
        let voices = voices.clamp(1, MAX_VOICES);
        shared.voices.store(voices as u32, Relaxed);
        Self {
            root,
            scale: PitchSet::from_semitones(scale.iter().copied()),
            // The bed's bass walks between the tonic and the fifth.
            scale_roots: PitchSet::from_semitones([0, 7]),
            chords,
            roots,
            voices,
            latched: [false; PADS],
            shared,
            ports: PortBuilder::default()
                .control_in_named(&GATES)
                .control_in_named(&["hold"])
                .control_in_named(&PITCHES[..voices])
                .control_out_named(&FREQS[..voices])
                .build(),
        }
    }

    /// The tones and roots in force for `fingers`, relative to `root`.
    fn sets(&self, fingers: [bool; PADS]) -> (PitchSet, PitchSet) {
        let (tones, roots) = self.chords.iter().zip(fingers).filter(|(_, on)| *on).fold(
            (PitchSet(0), PitchSet(0)),
            |(t, r), (c, _)| {
                (
                    t.union(c.tones),
                    r.union(PitchSet::from_semitones([c.root])),
                )
            },
        );
        if tones.is_empty() {
            (self.scale, self.scale_roots)
        } else {
            (tones, roots)
        }
    }

    pub fn render(&mut self, inputs: &Inputs, outputs: &mut [&mut [f32]]) {
        let block_size = outputs.first().map_or(0, |c| c.len());
        let high = |port: usize, i: usize| inputs[port].is_some_and(|x| x[i] >= 0.5);
        let mut notes = [0i32; MAX_VOICES];

        for i in 0..block_size {
            let curled: [bool; PADS] = std::array::from_fn(|f| high(port::GATE + f, i));
            // Holding keeps the latch; letting go follows the fingers again.
            if !high(port::HOLD, i)
                || (curled.iter().any(|&c| c) && !self.latched.iter().any(|&l| l))
            {
                self.latched = curled;
            }
            let (tones, roots) = self.sets(self.latched);

            for v in 0..self.voices {
                let pitch = inputs[port::PITCH + v].map_or(self.root as f32, |x| x[i]);
                let set = if v < self.roots { roots } else { tones };
                // Snap relative to the key, then back to an absolute note.
                let rel = pitch - self.root as f32;
                let note = set
                    .snap(rel)
                    .map_or(pitch.round() as i32, |n| n + self.root);
                notes[v] = note;
                outputs[v][i] = 440.0 * ((note - 69) as f32 / 12.0).exp2();
            }
        }

        let bits = self
            .latched
            .iter()
            .enumerate()
            .fold(0, |b, (f, &on)| b | (on as u32) << f);
        let hold = inputs[port::HOLD].is_some_and(|x| x.last().is_some_and(|&h| h >= 0.5));
        self.shared
            .fingers
            .store(bits | (hold as u32) << PADS, Relaxed);
        for (shared, &n) in self.shared.notes.iter().zip(&notes).take(self.voices) {
            shared.store(n.clamp(0, 127) as u32, Relaxed);
        }
    }
}

impl Node for Harmony {
    fn process(&mut self, _: &mut AudioContext, inputs: &Inputs, outputs: &mut [&mut [f32]]) {
        self.render(inputs, outputs);
    }
    fn ports(&self) -> &Ports {
        &self.ports
    }
}

/// i, VI, III, VII in A minor, each through its 9th: Am9, Fmaj9, Cmaj9, G9.
const DEFAULT_CHORDS: [[i32; 5]; PADS] = [
    [0, 3, 7, 10, 2],
    [8, 0, 3, 7, 10],
    [3, 7, 10, 2, 5],
    [10, 2, 5, 8, 0],
];
/// A minor pentatonic.
const DEFAULT_SCALE: [i32; 5] = [0, 3, 5, 7, 10];

impl NodeDefinition for Harmony {
    const NAME: &'static str = "harmony";
    const DESCRIPTION: &'static str =
        "Snaps generative pitches onto the chords held by the playing hand's fingers";
    const REQUIRED_PARAMS: &'static [&'static str] = &[];
    const OPTIONAL_PARAMS: &'static [&'static str] = &[
        "voices", "root", "scale", "finger0", "finger1", "finger2", "finger3", "roots",
    ];

    fn create(
        rb: &mut ResourceBuilderView,
        p: &DSLParams,
    ) -> Result<Box<dyn DynNode>, ValidationError> {
        let ints = |key: &str| -> Result<Option<Vec<i32>>, ValidationError> {
            Ok(p.get_array_f32(key)?
                .map(|v| v.iter().map(|x| x.round() as i32).collect()))
        };
        const FINGER_KEYS: [&str; PADS] = ["finger0", "finger1", "finger2", "finger3"];
        let mut chords = DEFAULT_CHORDS.map(|c| Chord::new(&c));
        for (chord, key) in chords.iter_mut().zip(FINGER_KEYS) {
            if let Some(notes) = ints(key)? {
                *chord = Chord::new(&notes);
            }
        }
        let scale = ints("scale")?.unwrap_or(DEFAULT_SCALE.to_vec());
        let voices = p.get_usize("voices")?.unwrap_or(4);
        if voices == 0 || voices > MAX_VOICES {
            return Err(ValidationError::InvalidParameter(format!(
                "harmony: voices must be 1..={MAX_VOICES}"
            )));
        }

        let root = p.get_f32("root")?.unwrap_or(57.0).round() as i32;
        let shared = Arc::new(Shared::new(root, &chords));
        rb.export_handle(shared.clone())?;
        Ok(Box::new(Self::new(
            voices,
            root,
            &scale,
            chords,
            p.get_usize("roots")?.unwrap_or(0),
            shared,
        )))
    }
}

impl ExportsHandle for Harmony {
    type Handle = Arc<Shared>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 16;

    fn node(voices: usize, roots: usize) -> Harmony {
        let chords = DEFAULT_CHORDS.map(|c| Chord::new(&c));
        let shared = Arc::new(Shared::new(57, &chords));
        Harmony::new(voices, 57, &DEFAULT_SCALE, chords, roots, shared)
    }

    #[test]
    fn default_chords_are_named_in_a_minor() {
        assert_eq!(node(1, 0).shared.names(), &["Am9", "Fmaj9", "Cmaj9", "G9"]);
        assert_eq!(Chord::new(&[0, 3, 7]).name(57), "Am");
        assert_eq!(Chord::new(&[0, 3, 7, 10]).name(57), "Am7");
        assert_eq!(Chord::new(&[0, 1]).name(60), "C?");
    }

    /// Runs one block with constant inputs and returns the MIDI note per voice.
    fn play(h: &mut Harmony, fingers: [bool; PADS], hold: bool, pitches: &[f32]) -> Vec<i32> {
        let on = |b: bool| vec![if b { 1.0 } else { 0.0 }; BLOCK];
        let mut bufs: Vec<Vec<f32>> = fingers.iter().map(|&f| on(f)).collect();
        bufs.push(on(hold));
        bufs.extend(pitches.iter().map(|&p| vec![p; BLOCK]));
        let inputs: Vec<Option<&[f32]>> = bufs.iter().map(|b| Some(b.as_slice())).collect();
        let mut out = vec![vec![0.0; BLOCK]; h.voices];
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|b| b.as_mut_slice()).collect();
        h.render(&inputs, &mut outs);
        (0..h.voices).map(|v| h.shared.note(v) as i32).collect()
    }

    #[test]
    fn snaps_to_nearest_member() {
        let triad = PitchSet::from_semitones([0, 4, 7]);
        assert_eq!(triad.snap(1.0), Some(0));
        assert_eq!(triad.snap(2.0), Some(0), "ties go down");
        assert_eq!(triad.snap(5.4), Some(4));
        assert_eq!(triad.snap(10.0), Some(12));
        assert_eq!(triad.snap(-1.0), Some(0));
        assert_eq!(PitchSet(0).snap(3.0), None);
    }

    #[test]
    fn no_fingers_plays_the_scale() {
        let mut h = node(1, 0);
        // A3 = 57; 58 (Bb) is not in A minor pentatonic, A is nearest.
        assert_eq!(play(&mut h, [false; 4], false, &[58.0]), [57]);
        assert_eq!(play(&mut h, [false; 4], false, &[61.6]), [62]);
    }

    #[test]
    fn a_curled_finger_plays_its_chord() {
        let mut h = node(1, 0);
        // Middle finger is Fmaj9: F A C E G. D ties C and E, going down;
        // F# ties F and G, going down.
        let f = [false, true, false, false];
        assert_eq!(play(&mut h, f, false, &[62.0]), [60]);
        assert_eq!(play(&mut h, f, false, &[66.0]), [65]);
        assert_eq!(play(&mut h, f, false, &[64.0]), [64]);
        assert!(h.shared.fingers()[1]);
    }

    #[test]
    fn bass_voices_take_chord_roots() {
        let mut h = node(2, 1);
        // Ring finger, Cmaj9: the bass goes to C, the other voice stays on E.
        let notes = play(&mut h, [false, false, true, false], false, &[50.0, 64.0]);
        assert_eq!(notes, [48, 64]);
    }

    #[test]
    fn hold_latches_the_chord() {
        let mut h = node(1, 0);
        let index = [true, false, false, false];
        play(&mut h, index, false, &[60.0]);
        // Thumb in, fingers open: Am9 stays.
        assert_eq!(play(&mut h, [false; 4], true, &[58.0]), [57]);
        assert_eq!(play(&mut h, [false; 4], true, &[62.0]), [60]);
        assert!(h.shared.fingers()[0] && h.shared.holding());
        // Thumb out: back to the scale.
        assert_eq!(play(&mut h, [false; 4], false, &[62.0]), [62]);
    }
}
