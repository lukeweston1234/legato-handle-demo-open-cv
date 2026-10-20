//! Turns raw landmarks into the handful of numbers the instrument plays.
//!
//! This runs on the network thread, never the audio thread, so it is free to
//! use trig, branch, and keep history. The audio side only ever sees the
//! resulting [`Gesture`].
//!
//! One hand plays: curling a finger opens its pad's gate, and tucking the
//! thumb across the palm holds whatever is looping. The other hand sculpts:
//! its position, pinch, rotation and distance become continuous controls.

use crate::protocol::{Frame, Hand, Handedness};

pub const PADS: usize = 4;

/// (knuckle, tip) for index, middle, ring and pinky.
const FINGERS: [(usize, usize); PADS] = [
    (Hand::INDEX_MCP, Hand::INDEX_TIP),
    (Hand::MIDDLE_MCP, Hand::MIDDLE_TIP),
    (Hand::RING_MCP, Hand::RING_TIP),
    (Hand::PINKY_MCP, Hand::PINKY_TIP),
];

/// One snapshot of both hands, already filtered and thresholded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gesture {
    /// Playing hand: which fingers (index..pinky) are curled.
    pub gates: [bool; PADS],
    /// How fast each finger was curling when its gate opened, 0..1.
    pub velocity: [f32; PADS],
    /// Playing hand's thumb is tucked across the palm.
    pub hold: bool,
    /// Sculpting hand, each 0..1: palm left to right, thumb–index spread,
    /// palm low to high, wrist rotation, and closeness to the camera.
    pub pos: f32,
    pub pinch: f32,
    pub height: f32,
    pub roll: f32,
    pub depth: f32,
    pub fist: bool,
}

impl Default for Gesture {
    /// Neutral controls: no pads, mid pinch, no transposition, no rotation.
    fn default() -> Self {
        Self {
            gates: [false; PADS],
            velocity: [0.0; PADS],
            hold: false,
            pos: 0.0,
            pinch: 0.5,
            height: 0.5,
            roll: 0.5,
            depth: 0.5,
            fist: false,
        }
    }
}

/// Every threshold and range in one place, so they can be tuned by playing.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub play_hand: Handedness,
    /// Camera width / height, so x and y distances mean the same thing.
    pub aspect: f32,
    /// Finger bend, in degrees between palm and finger, read as open and as curled.
    pub open_deg: f32,
    pub curled_deg: f32,
    /// Curl (0..1) that opens and closes a pad. The gap is the hysteresis.
    pub gate_on: f32,
    pub gate_off: f32,
    /// A finger curling at least `early_rate` per second opens its gate as
    /// soon as it passes `gate_early`, winning back roughly a camera frame.
    pub early_rate: f32,
    pub gate_early: f32,
    /// Curl speed, per second, that reads as full velocity.
    pub full_velocity_rate: f32,
    /// Thumb tip to ring knuckle, in palm lengths, that engages and releases hold.
    pub tuck_on: f32,
    pub tuck_off: f32,
    /// Mean curl of the sculpting hand that closes and opens the fist.
    pub fist_on: f32,
    pub fist_off: f32,
    /// Ranges mapped onto 0..1. `pos` and `height` are in image coordinates,
    /// `pinch` in palm lengths, `depth` in palm size on screen.
    pub pos_range: [f32; 2],
    pub height_range: [f32; 2],
    pub pinch_range: [f32; 2],
    pub depth_range: [f32; 2],
    /// Wrist rotation either side of level that reaches 0 or 1.
    pub roll_deg: f32,
    /// How long the playing hand may drop out before its pads release.
    pub grace_us: u64,
    /// One-euro filter for the continuous controls.
    pub min_cutoff_hz: f32,
    pub beta: f32,
}

impl Tuning {
    pub fn new(play_hand: Handedness) -> Self {
        // The image is mirrored, so the sculpting hand mostly lives on its own side.
        let pos_range = match play_hand {
            Handedness::Left => [0.4, 0.95],
            Handedness::Right => [0.05, 0.6],
        };
        Self {
            play_hand,
            aspect: 4.0 / 3.0,
            open_deg: 25.0,
            curled_deg: 110.0,
            gate_on: 0.6,
            gate_off: 0.35,
            early_rate: 6.0,
            gate_early: 0.3,
            full_velocity_rate: 10.0,
            tuck_on: 0.5,
            tuck_off: 0.7,
            fist_on: 0.75,
            fist_off: 0.5,
            pos_range,
            height_range: [0.15, 0.85],
            // A comfortably open hand, not a full stretch, reaches the top.
            pinch_range: [0.15, 1.1],
            depth_range: [0.08, 0.3],
            roll_deg: 75.0,
            grace_us: 120_000,
            min_cutoff_hz: 1.0,
            beta: 1.0,
        }
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self::new(Handedness::Left)
    }
}

pub struct GestureTracker {
    tuning: Tuning,
    gesture: Gesture,
    /// Raw curl per finger at the last playing-hand frame, for curl speed.
    curls: [f32; PADS],
    play_seen_us: Option<u64>,
    sculpt_seen_us: Option<u64>,
    /// Last palm x of each role, so a lone hand keeps the role it had.
    play_x: Option<f32>,
    sculpt_x: Option<f32>,
    filters: [OneEuro; 5],
}

impl GestureTracker {
    pub fn new(tuning: Tuning) -> Self {
        let filter = OneEuro::new(tuning.min_cutoff_hz, tuning.beta);
        Self {
            tuning,
            gesture: Gesture::default(),
            curls: [0.0; PADS],
            play_seen_us: None,
            sculpt_seen_us: None,
            play_x: None,
            sculpt_x: None,
            filters: [filter; 5],
        }
    }

    pub fn gesture(&self) -> Gesture {
        self.gesture
    }

    pub fn update(&mut self, frame: &Frame) -> Gesture {
        let now = frame.timestamp_us;
        let (play, sculpt) = self.assign(frame);

        match play {
            Some(hand) => self.play(hand, now),
            None if self
                .play_seen_us
                .is_some_and(|seen| now.saturating_sub(seen) > self.tuning.grace_us) =>
            {
                self.release()
            }
            None => {}
        }

        if let Some(hand) = sculpt {
            self.sculpt(hand, now);
        }

        self.gesture
    }

    /// Which hand plays and which sculpts, by where they are on screen.
    ///
    /// MediaPipe's left/right labels depend on whether the camera image is
    /// mirrored, and they flicker, so they are ignored. In the mirrored
    /// preview your left hand is on the left: with two hands the leftmost is
    /// your left. A lone hand keeps the role of whichever hand was last
    /// nearest, or goes by which half of the screen it's in.
    fn assign<'f>(&mut self, frame: &'f Frame) -> (Option<&'f Hand>, Option<&'f Hand>) {
        let mut hands: Vec<&Hand> = frame.hands.iter().collect();
        hands.sort_by(|a, b| b.score.total_cmp(&a.score));
        hands.truncate(2);
        hands.sort_by(|a, b| palm_x(a).total_cmp(&palm_x(b)));

        let plays_left = self.tuning.play_hand == Handedness::Left;
        let (play, sculpt) = match hands[..] {
            [] => (None, None),
            [left, right] if plays_left => (Some(left), Some(right)),
            [left, right] => (Some(right), Some(left)),
            [only] => {
                let x = palm_x(only);
                let near = |last: Option<f32>| last.map(|l| (x - l).abs());
                let on_play_side = (x < 0.5) == plays_left;
                let is_play = match (near(self.play_x), near(self.sculpt_x)) {
                    (Some(p), Some(s)) => p <= s,
                    (Some(p), None) => p < 0.25 || on_play_side,
                    (None, Some(s)) => s >= 0.25 && on_play_side,
                    (None, None) => on_play_side,
                };
                if is_play {
                    (Some(only), None)
                } else {
                    (None, Some(only))
                }
            }
            _ => unreachable!("truncated to two"),
        };
        if let Some(h) = play {
            self.play_x = Some(palm_x(h));
        }
        if let Some(h) = sculpt {
            self.sculpt_x = Some(palm_x(h));
        }
        (play, sculpt)
    }

    /// No packets at all: the tracker stopped or both hands left the frame.
    /// The pads release, but the sculpted controls keep their last values.
    pub fn lost(&mut self) -> Gesture {
        self.release();
        self.gesture
    }

    fn release(&mut self) {
        self.gesture.gates = [false; PADS];
        self.gesture.hold = false;
        self.play_seen_us = None;
    }

    fn play(&mut self, hand: &Hand, now: u64) {
        let t = self.tuning;
        let dt = self
            .play_seen_us
            .map(|seen| now.saturating_sub(seen) as f32 * 1e-6)
            .filter(|&dt| dt > 0.0 && dt < 0.25);
        self.play_seen_us = Some(now);

        let geo = Geometry::new(hand, t.aspect);
        for (pad, &(mcp, tip)) in FINGERS.iter().enumerate() {
            let curl = geo.curl(mcp, tip, &t);
            let rate = dt.map_or(0.0, |dt| (curl - self.curls[pad]) / dt);
            self.curls[pad] = curl;

            let gate = &mut self.gesture.gates[pad];
            if !*gate && (curl >= t.gate_on || (curl >= t.gate_early && rate >= t.early_rate)) {
                *gate = true;
                self.gesture.velocity[pad] = (rate / t.full_velocity_rate).clamp(0.15, 1.0);
            } else if *gate && curl < t.gate_off {
                *gate = false;
            }
        }

        let tuck = geo.dist(Hand::THUMB_TIP, Hand::RING_MCP) / geo.palm;
        let hold = &mut self.gesture.hold;
        if !*hold && tuck < t.tuck_on {
            *hold = true;
        } else if *hold && tuck > t.tuck_off {
            *hold = false;
        }
    }

    fn sculpt(&mut self, hand: &Hand, now: u64) {
        let t = self.tuning;
        let dt = self
            .sculpt_seen_us
            .map(|seen| now.saturating_sub(seen) as f32 * 1e-6)
            .filter(|&dt| dt > 0.0 && dt < 0.25);
        self.sculpt_seen_us = Some(now);

        let geo = Geometry::new(hand, t.aspect);
        let [cx, cy, _] = geo.palm_centre_raw(hand);

        // Level is the knuckle line pointing away from the thumb, whichever hand this is.
        let [kx, ky, _] = sub(geo.p[Hand::PINKY_MCP], geo.p[Hand::INDEX_MCP]);
        let kx = if self.tuning.play_hand == Handedness::Right {
            -kx
        } else {
            kx
        };
        let roll_deg = ky.atan2(kx).to_degrees();

        let raw = [
            remap(cx, t.pos_range),
            1.0 - remap(cy, t.height_range),
            remap(
                geo.dist(Hand::THUMB_TIP, Hand::INDEX_TIP) / geo.palm,
                t.pinch_range,
            ),
            (0.5 + roll_deg / (2.0 * t.roll_deg)).clamp(0.0, 1.0),
            remap(geo.palm, t.depth_range),
        ];
        let [pos, height, pinch, roll, depth] =
            std::array::from_fn(|i| self.filters[i].filter(raw[i], dt));

        let g = &mut self.gesture;
        (g.pos, g.height, g.pinch, g.roll, g.depth) = (pos, height, pinch, roll, depth);

        let mean_curl = FINGERS
            .iter()
            .map(|&(mcp, tip)| geo.curl(mcp, tip, &t))
            .sum::<f32>()
            / PADS as f32;
        if !g.fist && mean_curl >= t.fist_on {
            g.fist = true;
        } else if g.fist && mean_curl < t.fist_off {
            g.fist = false;
        }
    }
}

const PALM: [usize; 5] = [
    Hand::WRIST,
    Hand::INDEX_MCP,
    Hand::MIDDLE_MCP,
    Hand::RING_MCP,
    Hand::PINKY_MCP,
];

/// Palm centre x in image coordinates.
fn palm_x(hand: &Hand) -> f32 {
    PALM.iter().map(|&i| hand.landmarks[i][0]).sum::<f32>() / PALM.len() as f32
}

/// Landmarks with x and z scaled into the same units as y.
struct Geometry {
    p: [[f32; 3]; crate::LANDMARKS],
    /// Wrist to middle knuckle: the unit every proportion is measured in.
    palm: f32,
}

impl Geometry {
    fn new(hand: &Hand, aspect: f32) -> Self {
        let p = hand.landmarks.map(|[x, y, z]| [x * aspect, y, z * aspect]);
        let palm = len(sub(p[Hand::MIDDLE_MCP], p[Hand::WRIST])).max(1e-4);
        Self { p, palm }
    }

    fn dist(&self, a: usize, b: usize) -> f32 {
        len(sub(self.p[a], self.p[b]))
    }

    /// The angle between the palm (wrist to knuckle) and the finger (knuckle
    /// to tip), mapped to 0..1. Angles don't depend on hand size or how far
    /// the hand is from the camera, and they catch a finger curling towards
    /// the lens, which barely moves in x and y.
    fn curl(&self, mcp: usize, tip: usize, t: &Tuning) -> f32 {
        let palm = sub(self.p[mcp], self.p[Hand::WRIST]);
        let finger = sub(self.p[tip], self.p[mcp]);
        let cos = dot(palm, finger) / (len(palm) * len(finger)).max(1e-6);
        let deg = cos.clamp(-1.0, 1.0).acos().to_degrees();
        ((deg - t.open_deg) / (t.curled_deg - t.open_deg)).clamp(0.0, 1.0)
    }

    /// Mean of the wrist and four knuckles, in unscaled image coordinates.
    fn palm_centre_raw(&self, hand: &Hand) -> [f32; 3] {
        let mut c = [0.0; 3];
        for i in PALM {
            for (axis, v) in c.iter_mut().zip(hand.landmarks[i]) {
                *axis += v / PALM.len() as f32;
            }
        }
        c
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn len(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn remap(v: f32, [lo, hi]: [f32; 2]) -> f32 {
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// The 1€ filter: heavy smoothing while the hand is still, little lag once it moves.
///
/// <https://gery.casiez.net/1euro/>
#[derive(Debug, Clone, Copy)]
struct OneEuro {
    min_cutoff_hz: f32,
    beta: f32,
    x: Option<f32>,
    dx: f32,
}

impl OneEuro {
    const D_CUTOFF_HZ: f32 = 1.0;

    fn new(min_cutoff_hz: f32, beta: f32) -> Self {
        Self {
            min_cutoff_hz,
            beta,
            x: None,
            dx: 0.0,
        }
    }

    /// `dt` is `None` for the first sample, or after a gap long enough that
    /// smoothing towards the stale value would only add lag.
    fn filter(&mut self, v: f32, dt: Option<f32>) -> f32 {
        let (Some(x), Some(dt)) = (self.x, dt) else {
            self.x = Some(v);
            self.dx = 0.0;
            return v;
        };
        let alpha = |cutoff: f32| 1.0 / (1.0 + 1.0 / (std::f32::consts::TAU * cutoff * dt));

        self.dx += alpha(Self::D_CUTOFF_HZ) * ((v - x) / dt - self.dx);
        let cutoff = self.min_cutoff_hz + self.beta * self.dx.abs();
        let x = x + alpha(cutoff) * (v - x);
        self.x = Some(x);
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LANDMARKS;

    const FRAME_US: u64 = 33_000;

    /// A flat hand, palm to the camera, fingers pointing up, centred at `x`.
    /// `curls` bends each finger (index..pinky) from straight (0°) towards the camera.
    fn hand(side: Handedness, x: f32, curls_deg: [f32; PADS], thumb_tucked: bool) -> Hand {
        let mut lm = [[0.0f32; 3]; LANDMARKS];
        let y0 = 0.7;
        lm[Hand::WRIST] = [x, y0, 0.0];
        // Knuckles in a row 0.15 above the wrist (in y units), spread across x.
        let mcp_y = y0 - 0.15;
        for (i, &(mcp, tip)) in FINGERS.iter().enumerate() {
            let kx = x + (i as f32 - 1.5) * 0.03;
            lm[mcp] = [kx, mcp_y, 0.0];
            let a = curls_deg[i].to_radians();
            // The finger leaves the knuckle along the palm's direction (up, -y),
            // rotated towards the camera (-z) by `a`.
            let palm_dir = [(kx - x) / 1.0, mcp_y - y0, 0.0];
            let n = len(palm_dir);
            let up = [palm_dir[0] / n, palm_dir[1] / n, 0.0];
            let length = 0.1;
            lm[tip] = [
                kx + up[0] * a.cos() * length,
                mcp_y + up[1] * a.cos() * length,
                -a.sin() * length,
            ];
        }
        lm[Hand::THUMB_TIP] = if thumb_tucked {
            lm[Hand::RING_MCP]
        } else {
            [x - 0.12, y0 - 0.05, 0.0]
        };
        // Leave x unscaled-correct: tests use aspect 1.0.
        Hand {
            handedness: side,
            score: 0.9,
            landmarks: lm,
        }
    }

    fn tracker() -> GestureTracker {
        GestureTracker::new(Tuning {
            aspect: 1.0,
            ..Tuning::default()
        })
    }

    fn frame(n: u64, hands: Vec<Hand>) -> Frame {
        Frame {
            timestamp_us: n * FRAME_US,
            hands,
        }
    }

    fn left(curls: [f32; PADS]) -> Hand {
        hand(Handedness::Left, 0.3, curls, false)
    }

    #[test]
    fn straight_fingers_read_open_and_bent_fingers_read_curled() {
        let t = Tuning {
            aspect: 1.0,
            ..Tuning::default()
        };
        let geo = Geometry::new(&left([0.0, 60.0, 120.0, 180.0]), 1.0);
        let curls: Vec<f32> = FINGERS
            .iter()
            .map(|&(m, tip)| geo.curl(m, tip, &t))
            .collect();
        assert_eq!(curls[0], 0.0);
        assert!((0.3..0.5).contains(&curls[1]), "{curls:?}");
        assert_eq!(curls[2], 1.0);
        assert_eq!(curls[3], 1.0);
    }

    #[test]
    fn curling_a_finger_opens_only_its_gate_with_hysteresis() {
        let mut tr = tracker();
        tr.update(&frame(0, vec![left([0.0; PADS])]));

        // Slowly, so the early trigger doesn't fire: 90° is past gate_on.
        let g = tr.update(&frame(10, vec![left([0.0, 90.0, 0.0, 0.0])]));
        assert_eq!(g.gates, [false, true, false, false]);

        // Easing back to 60° (curl ≈ 0.41) sits inside the hysteresis band.
        let g = tr.update(&frame(20, vec![left([0.0, 60.0, 0.0, 0.0])]));
        assert!(g.gates[1]);

        let g = tr.update(&frame(30, vec![left([0.0; PADS])]));
        assert_eq!(g.gates, [false; PADS]);
    }

    #[test]
    fn a_fast_flick_opens_early_and_loud() {
        let mut tr = tracker();
        tr.update(&frame(0, vec![left([0.0; PADS])]));
        // 55° is below gate_on, but getting there in one frame is fast.
        let g = tr.update(&frame(1, vec![left([55.0, 0.0, 0.0, 0.0])]));
        assert!(g.gates[0]);
        assert!(g.velocity[0] > 0.9, "{}", g.velocity[0]);
    }

    #[test]
    fn a_slow_curl_is_quiet() {
        let mut tr = tracker();
        for (n, deg) in (0..=20).map(|n| (n, n as f32 * 4.5)) {
            tr.update(&frame(n, vec![left([deg, 0.0, 0.0, 0.0])]));
        }
        let g = tr.gesture();
        assert!(g.gates[0]);
        assert!(g.velocity[0] < 0.4, "{}", g.velocity[0]);
    }

    #[test]
    fn tucked_thumb_holds() {
        let mut tr = tracker();
        let g = tr.update(&frame(
            0,
            vec![hand(Handedness::Left, 0.3, [0.0; PADS], true)],
        ));
        assert!(g.hold);
        let g = tr.update(&frame(1, vec![left([0.0; PADS])]));
        assert!(!g.hold);
    }

    #[test]
    fn pads_survive_a_dropped_frame_but_not_a_lost_hand() {
        let mut tr = tracker();
        tr.update(&frame(0, vec![left([0.0; PADS])]));
        tr.update(&frame(10, vec![left([90.0; PADS])]));

        // One frame without the playing hand is inside the grace period.
        let g = tr.update(&frame(11, vec![]));
        assert_eq!(g.gates, [true; PADS]);

        let g = tr.update(&frame(20, vec![]));
        assert_eq!(g.gates, [false; PADS]);

        tr.update(&frame(30, vec![left([90.0; PADS])]));
        assert_eq!(tr.lost().gates, [false; PADS]);
    }

    #[test]
    fn sculpting_hand_moves_pos_and_keeps_it_when_gone() {
        let mut tr = tracker();
        let right = |x| hand(Handedness::Right, x, [0.0; PADS], false);

        let g = tr.update(&frame(0, vec![right(0.5)]));
        let low = g.pos;
        // Hold still at the far side so the filter settles.
        for n in 1..30 {
            tr.update(&frame(n, vec![right(0.9)]));
        }
        let high = tr.gesture().pos;
        assert!(low < 0.2 && high > 0.85, "{low} {high}");

        let g = tr.update(&frame(31, vec![]));
        assert_eq!(g.pos, high);
    }

    #[test]
    fn sculpting_fist_closes_with_hysteresis() {
        let mut tr = tracker();
        let right = |deg| hand(Handedness::Right, 0.7, [deg; PADS], false);
        assert!(tr.update(&frame(0, vec![right(150.0)])).fist);
        assert!(tr.update(&frame(1, vec![right(85.0)])).fist);
        assert!(!tr.update(&frame(2, vec![right(30.0)])).fist);
    }

    #[test]
    fn labels_are_ignored_and_the_leftmost_hand_plays() {
        let mut tr = tracker();
        // MediaPipe calls both hands "Right"; the one on the left still plays.
        let g = tr.update(&frame(
            0,
            vec![
                hand(Handedness::Right, 0.8, [0.0; PADS], false),
                hand(Handedness::Right, 0.3, [90.0; PADS], false),
            ],
        ));
        assert_eq!(g.gates, [true; PADS]);
    }

    #[test]
    fn a_lone_hand_keeps_its_role_across_the_middle() {
        let mut tr = tracker();
        let both = |n| {
            frame(
                n,
                vec![
                    left([0.0; PADS]),
                    hand(Handedness::Right, 0.7, [0.0; PADS], false),
                ],
            )
        };
        tr.update(&both(0));

        // The sculpting hand drifts left past the middle on its own: still sculpting.
        let lone = |n, x| frame(n, vec![hand(Handedness::Right, x, [90.0; PADS], false)]);
        for (n, x) in [0.65, 0.6, 0.55, 0.5, 0.45, 0.4].into_iter().enumerate() {
            let g = tr.update(&lone(n as u64 + 1, x));
            assert_eq!(g.gates, [false; PADS], "at x {x}");
            assert!(g.fist, "its curled fingers read as a fist, not pads");
        }
    }

    #[test]
    fn swapped_hands_play_with_the_right() {
        let mut tr = GestureTracker::new(Tuning {
            aspect: 1.0,
            ..Tuning::new(Handedness::Right)
        });
        let g = tr.update(&frame(
            0,
            vec![
                left([90.0; PADS]),
                hand(Handedness::Right, 0.7, [0.0; PADS], false),
            ],
        ));
        assert_eq!(g.gates, [false; PADS]);
    }

    #[test]
    fn one_euro_passes_steady_values_and_resets_after_gaps() {
        let mut f = OneEuro::new(1.0, 1.0);
        assert_eq!(f.filter(0.2, None), 0.2);
        let mut y = 0.0;
        for _ in 0..200 {
            y = f.filter(0.8, Some(0.033));
        }
        assert!((y - 0.8).abs() < 1e-3);
        assert_eq!(f.filter(0.1, None), 0.1);
    }
}
