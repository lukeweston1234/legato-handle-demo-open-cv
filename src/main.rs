//! Marionette's audio process: hands in over UDP from `tracking.py`, sound out
//! over JACK, and a viewer window for tuning the slicer's bounds.
//!
//!     cargo run --release -- <sample> [--graph marionette.legato] [--addr 127.0.0.1:9870] [--play-right] [--no-ui]

use std::{
    error::Error,
    io::{ErrorKind, Write},
    net::UdpSocket,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use legato::{prelude::*, resources::buffer::decode_with_ffmpeg};
use legato_hand_tracking::{
    Frame, Handedness,
    bounds::Preset,
    build,
    gesture::{Gesture, GestureTracker, PADS, Tuning},
    nodes::{Hands, Slicer},
    viewer::{self, ViewerArgs},
};

/// `tracking.py` only sends frames with a hand in them, so silence this long
/// means both hands have left the camera.
const LOST_AFTER: Duration = Duration::from_millis(120);
const METER_EVERY: Duration = Duration::from_millis(50);

struct Args {
    sample: PathBuf,
    graph: PathBuf,
    addr: String,
    play_hand: Handedness,
    ui: bool,
}

impl Args {
    const USAGE: &str = "usage: legato-hand-tracking <sample> [--graph FILE] [--addr HOST:PORT] [--play-right] [--no-ui]";

    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut sample = None;
        let mut graph = PathBuf::from("marionette.legato");
        let mut addr = String::from("127.0.0.1:9870");
        let mut play_hand = Handedness::Left;
        let mut ui = true;

        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or(format!("{arg} needs a value\n{}", Self::USAGE))
            };
            match arg.as_str() {
                "--graph" => graph = value()?.into(),
                "--addr" => addr = value()?,
                "--play-right" => play_hand = Handedness::Right,
                "--no-ui" => ui = false,
                "-h" | "--help" => return Err(Self::USAGE.into()),
                _ if sample.is_none() && !arg.starts_with('-') => sample = Some(arg.into()),
                _ => return Err(format!("unexpected argument '{arg}'\n{}", Self::USAGE)),
            }
        }

        Ok(Self {
            sample: sample.ok_or(Self::USAGE)?,
            graph,
            addr,
            play_hand,
            ui,
        })
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;

    let config = Config {
        sample_rate: env_or("LEGATO_SAMPLE_RATE", 48_000),
        // Small blocks keep gates tight; the camera is the real latency.
        block_size: env_or("LEGATO_BLOCK_SIZE", 256),
        channels: 2,
        rt_capacity: 0,
    };

    let graph = std::fs::read_to_string(&args.graph)
        .map_err(|e| format!("could not read {}: {e}", args.graph.display()))?;
    let (app, mut frontend) = build(config, &graph)
        .map_err(|e| format!("{} did not build: {e:?}", args.graph.display()))?;

    let hands = frontend.take_handle_of::<Hands>("hands")?;
    let slicer = frontend.take_handle_of::<Slicer>("slicer")?;
    let sample_rate = config.sample_rate as u32;
    frontend.load_sample("main", Path::new(&args.sample), 2, sample_rate)?;

    // The viewer draws its own mono copy; the graph's buffer is out of reach.
    let mono = decode_with_ffmpeg(&args.sample, 1, sample_rate)?;
    let secs = mono.len_samples() as f32 / sample_rate as f32;
    let (preset, saved) = viewer::load_preset(&args.sample, slicer.bounds(), secs)?;
    slicer.set_bounds(preset.bounds);
    if saved {
        eprintln!("bounds from {}", Preset::path_for(&args.sample).display());
    }

    let socket = UdpSocket::bind(&args.addr)?;
    socket.set_read_timeout(Some(LOST_AFTER))?;
    let tracker = GestureTracker::new(Tuning::new(args.play_hand));
    eprintln!("listening for hands on {}", args.addr);
    std::thread::spawn(move || listen(socket, tracker, hands));

    let host = cpal::host_from_id(cpal::HostId::Jack)?;
    let interface = AudioInterface::builder(&host, config)
        .build(app)
        .map_err(|e| {
            // cpal reads the JACK buffer size on connect, before PipeWire would
            // honour a per-client latency, so the graph quantum has to be forced.
            format!(
                "{e}\n\nOn PipeWire, force the quantum to the block size first:\n    \
             pw-metadata -n settings 0 clock.force-quantum {}\n\
             and set it back to 0 when you're done.",
                config.block_size
            )
        })?;

    if !args.ui {
        interface.run_forever();
    }
    // Windowing wants the main thread; the audio runs until the window closes.
    viewer::run(ViewerArgs {
        frontend,
        shared: slicer,
        clock: "grid".into(),
        mono: mono.data.to_vec(),
        sample_rate: sample_rate as f32,
        sample: args.sample,
        preset,
        saved,
    })?;
    drop(interface);
    Ok(())
}

fn listen(socket: UdpSocket, mut tracker: GestureTracker, mut tx: rtrb::Producer<Gesture>) {
    // Largest frame we expect is a handful of hands; 4 KiB is plenty.
    let mut buf = [0u8; 4096];
    let mut meter = Meter::default();

    loop {
        let gesture = match socket.recv(&mut buf) {
            Ok(len) => match Frame::decode(&buf[..len]) {
                Ok(frame) => tracker.update(&frame),
                Err(e) => {
                    eprintln!("bad packet: {e}");
                    continue;
                }
            },
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                tracker.lost()
            }
            Err(e) => {
                eprintln!("socket closed: {e}");
                return;
            }
        };
        // Full means the audio thread has stalled; a newer gesture follows shortly.
        let _ = tx.push(gesture);
        meter.show(&gesture);
    }
}

/// A one-line readout of what the instrument hears, redrawn in place.
#[derive(Default)]
struct Meter {
    last: Option<Instant>,
}

impl Meter {
    fn show(&mut self, g: &Gesture) {
        if self.last.is_some_and(|t| t.elapsed() < METER_EVERY) {
            return;
        }
        self.last = Some(Instant::now());

        let pads: String = (0..PADS)
            .map(|p| if g.gates[p] { '■' } else { '·' })
            .collect();
        let mut err = std::io::stderr().lock();
        let _ = write!(
            err,
            "\r\x1b[2Kpads {pads} {hold}  pos {:.2}  len {:.2}  height {:.2}  roll {:.2}  depth {:.2}{fist}",
            g.pos,
            g.pinch,
            g.height,
            g.roll,
            g.depth,
            hold = if g.hold { "hold" } else { "    " },
            fist = if g.fist { "  fist" } else { "" },
        );
        let _ = err.flush();
    }
}
