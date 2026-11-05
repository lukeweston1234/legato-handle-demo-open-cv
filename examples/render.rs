//! Renders the patch offline with a scripted performance, to hear it without
//! a camera or JACK:
//!
//!     cargo run --release --example render -- out.wav [--graph marionette.legato]
//!
//! Eight seconds on its own, then each finger's chord in turn: sparse, flat
//! out with a short arp, flat out with a slow arp, drenched in reverb, and a
//! fist.

use std::{
    error::Error,
    io::Write,
    process::{Command, Stdio},
};

use legato::prelude::*;
use legato_hand_tracking::{build, gesture::Gesture, nodes::Hands};

const SR: usize = 48_000;
const BLOCK: usize = 256;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .ok_or("usage: render <out.wav> [--graph FILE]")?;
    let graph = match (args.next().as_deref(), args.next()) {
        (Some("--graph"), Some(path)) => path,
        _ => "marionette.legato".into(),
    };

    let config = Config {
        sample_rate: SR,
        block_size: BLOCK,
        channels: 2,
        rt_capacity: 0,
    };
    let (mut app, mut frontend) = build(config, &std::fs::read_to_string(&graph)?)
        .map_err(|e| format!("{graph} did not build: {e:?}"))?;
    let mut hands = frontend.take_handle_of::<Hands>("hands")?;

    let finger = |f: usize| {
        let mut g = Gesture::default();
        g.gates[f] = true;
        g
    };
    let sculpt = |f: usize, height: f32, roll: f32, pos: f32, depth: f32| Gesture {
        height,
        roll,
        pos,
        depth,
        ..finger(f)
    };
    // (seconds, gesture)
    let script = [
        (8.0, Gesture::default()),
        (6.0, sculpt(0, 0.1, 0.0, 0.2, 0.3)),
        // Flat out, with a short, plucky arp...
        (6.0, sculpt(1, 1.0, 0.0, 0.0, 0.3)),
        // ...and with a slow, swelling one.
        (6.0, sculpt(2, 1.0, 1.0, 1.0, 0.3)),
        // Leaning right in.
        (6.0, sculpt(3, 0.7, 0.2, 0.5, 1.0)),
        (
            6.0,
            Gesture {
                fist: true,
                ..sculpt(3, 0.7, 0.2, 0.5, 1.0)
            },
        ),
        (6.0, Gesture::default()),
    ];

    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "f32le",
            "-ar",
            &SR.to_string(),
            "-ac",
            "2",
            "-i",
            "-",
        ])
        .arg(&out)
        .stdin(Stdio::piped())
        .spawn()?;
    let stdin = ffmpeg.stdin.as_mut().ok_or("no ffmpeg stdin")?;
    let mut peak = 0.0f32;
    let mut bytes = Vec::with_capacity(BLOCK * 8);

    for (secs, gesture) in script {
        hands.push(gesture).map_err(|_| "hands queue full")?;
        for _ in 0..(secs * SR as f32 / BLOCK as f32) as usize {
            let block = app.next_block();
            bytes.clear();
            for i in 0..BLOCK {
                for c in &block.channels[..block.chans] {
                    peak = peak.max(c[i].abs());
                    bytes.extend(c[i].to_le_bytes());
                }
            }
            stdin.write_all(&bytes)?;
        }
    }
    drop(ffmpeg.stdin.take());
    ffmpeg.wait()?;
    eprintln!("wrote {out}, peak {peak:.2}");
    Ok(())
}
