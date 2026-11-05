//! The shipped patch builds, exports its handles, and plays.

use legato::prelude::*;
use legato_hand_tracking::{
    build,
    gesture::Gesture,
    nodes::{Hands, Harmony},
};

const GRAPH: &str = include_str!("../marionette.legato");
const SR: usize = 48_000;
const BLOCK: usize = 64;

fn config() -> Config {
    Config {
        sample_rate: SR,
        block_size: BLOCK,
        channels: 2,
        rt_capacity: 0,
    }
}

/// Peak output over `secs` of audio, checking every sample is finite.
fn peak(app: &mut LegatoApp, secs: f32) -> f32 {
    let blocks = (secs * SR as f32 / BLOCK as f32) as usize;
    let mut peak = 0.0f32;
    for _ in 0..blocks {
        let block = app.next_block();
        assert_eq!(block.chans, 2);
        for c in &block.channels[..block.chans] {
            for &y in c.iter() {
                assert!(y.is_finite());
                peak = peak.max(y.abs());
            }
        }
    }
    peak
}

#[test]
fn marionette_patch_builds_and_exports_its_handles() {
    let (_app, frontend) = build(config(), GRAPH).expect("patch should build");
    let mut paths = frontend.handle_paths();
    paths.sort();
    assert_eq!(paths, ["hands", "harmony"]);
}

/// With nobody in front of the camera the band still plays, and stays sane.
#[test]
fn it_plays_on_its_own() {
    let (mut app, mut frontend) = build(config(), GRAPH).unwrap();
    let harmony = frontend.take_handle_of::<Harmony>("harmony").unwrap();
    let p = peak(&mut app, 6.0);
    assert!(p > 0.01, "the band plays: {p}");
    assert!(p < 1.0, "and doesn't clip: {p}");
    assert_eq!(harmony.fingers(), [false; 4]);
}

/// A curled finger reaches the harmony node, and its chord is what sounds.
#[test]
fn a_curled_finger_sets_the_chord() {
    let (mut app, mut frontend) = build(config(), GRAPH).unwrap();
    let mut hands = frontend.take_handle_of::<Hands>("hands").unwrap();
    let harmony = frontend.take_handle_of::<Harmony>("harmony").unwrap();

    let mut g = Gesture::default();
    g.gates[2] = true; // ring finger: Cmaj9
    hands.push(g).unwrap();
    peak(&mut app, 3.0);

    assert_eq!(harmony.fingers(), [false, false, true, false]);
    for v in 0..harmony.voices() {
        let pc = harmony.note(v) % 12;
        assert!(
            [0, 4, 7, 11, 2].contains(&pc),
            "voice {v} plays pitch class {pc}"
        );
    }
    assert_eq!(harmony.note(0) % 12, 0, "the bass is on the root");
}
