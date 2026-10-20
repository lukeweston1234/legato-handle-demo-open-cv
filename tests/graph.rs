//! The shipped patch builds, exports its handles, and runs.

use legato::{
    msg::{NodeMessage, ParamPayload, RtValue},
    prelude::*,
};
use legato_hand_tracking::{
    build,
    gesture::Gesture,
    nodes::{Hands, Slicer},
};

const GRAPH: &str = include_str!("../marionette.legato");

fn config() -> Config {
    Config {
        sample_rate: 48_000,
        block_size: 64,
        channels: 2,
        rt_capacity: 0,
    }
}

#[test]
fn marionette_patch_builds_and_exports_its_handles() {
    let (mut app, mut frontend) = build(config(), GRAPH).expect("patch should build");
    let mut paths = frontend.handle_paths();
    paths.sort();
    assert_eq!(paths, ["hands", "slicer"]);
    // The viewer retunes the clock by this name.
    assert!(frontend.clone_registry().contains_key("grid"));

    let mut hands = frontend
        .take_handle_of::<Hands>("hands")
        .expect("hands handle");
    frontend
        .take_handle_of::<Slicer>("slicer")
        .expect("slicer handle");
    frontend
        .send_node_msg(
            "grid",
            NodeMessage::SetParam(ParamPayload {
                param_name: "bpm",
                value: RtValue::F32(135.0),
            }),
        )
        .expect("the clock takes a bpm message");

    let mut g = Gesture::default();
    g.gates[0] = true;
    hands.push(g).unwrap();

    // No sample is loaded, so the slicer stays silent, but every node runs.
    for _ in 0..100 {
        let block = app.next_block();
        assert!(
            block
                .channels
                .iter()
                .all(|c| c.iter().all(|y| y.is_finite()))
        );
    }
}

/// A real sample through ffmpeg, a tap through the handle, and sound out of
/// the whole patch: the path `main` takes, minus the camera and JACK.
#[test]
fn a_tapped_pad_makes_sound_through_the_patch() {
    let dir = std::env::temp_dir().join(format!("marionette-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("tone.wav");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=220:duration=2",
        ])
        .arg(&wav)
        .status()
        .expect("ffmpeg should be on PATH (it is in the dev shell)");
    assert!(status.success());

    let (mut app, mut frontend) = build(config(), GRAPH).unwrap();
    let mut hands = frontend.take_handle_of::<Hands>("hands").unwrap();
    frontend.load_sample("main", &wav, 2, 48_000).unwrap();

    let peak = |app: &mut LegatoApp, blocks: usize| {
        (0..blocks)
            .map(|_| {
                let block = app.next_block();
                block
                    .channels
                    .iter()
                    .flat_map(|c| c.iter())
                    .fold(0.0f32, |m, y| m.max(y.abs()))
            })
            .fold(0.0, f32::max)
    };

    assert_eq!(peak(&mut app, 50), 0.0, "silent before any pad");

    let mut g = Gesture::default();
    g.gates[1] = true;
    g.velocity[1] = 1.0;
    hands.push(g).unwrap();

    // A 16th at 120 BPM is 125 ms, about 94 blocks: the tap lands within it.
    assert!(peak(&mut app, 200) > 0.05, "the pad sounds");
    std::fs::remove_dir_all(dir).ok();
}
