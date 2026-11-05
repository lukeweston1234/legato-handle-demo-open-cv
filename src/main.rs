//! Marionette's audio process: hands in over UDP from `tracking.py`, sound out
//! over JACK, and a status packet back to the tracker for its overlay.
//!
//!     cargo run --release -- [--graph marionette.legato] [--addr 127.0.0.1:9870] [--play-right]
//!
//! There is no window here: the audio thread and one network thread are all
//! that run, so a small box keeps its whole budget for the graph.

use std::{
    error::Error,
    io::{ErrorKind, Write},
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use legato::prelude::*;
use legato_hand_tracking::{
    Frame, Handedness, build,
    gesture::{Gesture, GestureTracker, PADS, Tuning},
    nodes::{Hands, Harmony, harmony},
    status::Status,
};

/// `tracking.py` sends every camera frame, hands or not, so silence this
/// long means the tracker has stopped.
const LOST_AFTER: Duration = Duration::from_millis(120);
const METER_EVERY: Duration = Duration::from_millis(50);
/// About the camera's frame rate; the overlay can't show more than that.
const STATUS_EVERY: Duration = Duration::from_millis(30);

struct Args {
    graph: PathBuf,
    addr: String,
    play_hand: Handedness,
}

impl Args {
    const USAGE: &str =
        "usage: legato-hand-tracking [--graph FILE] [--addr HOST:PORT] [--play-right]";

    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut graph = PathBuf::from("marionette.legato");
        let mut addr = String::from("127.0.0.1:9870");
        let mut play_hand = Handedness::Left;

        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or(format!("{arg} needs a value\n{}", Self::USAGE))
            };
            match arg.as_str() {
                "--graph" => graph = value()?.into(),
                "--addr" => addr = value()?,
                "--play-right" => play_hand = Handedness::Right,
                "-h" | "--help" => return Err(Self::USAGE.into()),
                _ => return Err(format!("unexpected argument '{arg}'\n{}", Self::USAGE)),
            }
        }

        Ok(Self {
            graph,
            addr,
            play_hand,
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
        block_size: env_or("LEGATO_BLOCK_SIZE", 1024),
        channels: 2,
        rt_capacity: 0,
    };

    let graph = std::fs::read_to_string(&args.graph)
        .map_err(|e| format!("could not read {}: {e}", args.graph.display()))?;
    let (app, mut frontend) = build(config, &graph)
        .map_err(|e| format!("{} did not build: {e:?}", args.graph.display()))?;

    let hands = frontend.take_handle_of::<Hands>("hands")?;
    let harmony = frontend.take_handle_of::<Harmony>("harmony")?;

    let socket = UdpSocket::bind(&args.addr)?;
    socket.set_read_timeout(Some(LOST_AFTER))?;
    let tracker = GestureTracker::new(Tuning::new(args.play_hand));
    eprintln!("listening for hands on {}", args.addr);
    std::thread::spawn(move || listen(socket, tracker, hands, harmony));

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

    // Nothing else needs the main thread; the network thread runs on its own.
    interface.run_forever();
}

fn listen(
    socket: UdpSocket,
    mut tracker: GestureTracker,
    mut tx: rtrb::Producer<Gesture>,
    harmony: Arc<harmony::Shared>,
) {
    // Largest frame we expect is a handful of hands; 4 KiB is plenty.
    let mut buf = [0u8; 4096];
    let mut meter = Meter::default();
    // The tracker's own socket, which is where its overlay listens.
    let mut peer: Option<SocketAddr> = None;
    let mut status = Vec::new();
    let mut sent: Option<Instant> = None;

    loop {
        let gesture = match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                peer = Some(from);
                match Frame::decode(&buf[..len]) {
                    Ok(frame) => tracker.update(&frame),
                    Err(e) => {
                        eprintln!("bad packet: {e}");
                        continue;
                    }
                }
            }
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
        meter.show(&gesture, &harmony);

        if let Some(peer) = peer
            && sent.is_none_or(|t| t.elapsed() >= STATUS_EVERY)
        {
            sent = Some(Instant::now());
            Status::new(&gesture, &harmony).encode(&mut status);
            // The tracker may have gone; it will say hello again when it's back.
            let _ = socket.send_to(&status, peer);
        }
    }
}

/// A one-line readout of what the instrument hears, redrawn in place.
#[derive(Default)]
struct Meter {
    last: Option<Instant>,
}

impl Meter {
    fn show(&mut self, g: &Gesture, harmony: &harmony::Shared) {
        if self.last.is_some_and(|t| t.elapsed() < METER_EVERY) {
            return;
        }
        self.last = Some(Instant::now());

        let sounding = harmony.fingers();
        let chord: Vec<&str> = (0..PADS)
            .filter(|&f| sounding[f])
            .map(|f| harmony.names()[f].as_str())
            .collect();
        let chord = if chord.is_empty() {
            "pentatonic".to_string()
        } else {
            chord.join("+")
        };
        let mut err = std::io::stderr().lock();
        let _ = write!(
            err,
            "\r\x1b[2K{chord:<24}{hold}  busy {:.2}  tone {:.2}  attack {:.2}  decay {:.2}  verb {:.2}{fist}",
            g.height,
            g.pinch,
            g.roll,
            g.pos,
            g.depth,
            hold = if harmony.holding() { "hold" } else { "    " },
            fist = if g.fist { "  hush" } else { "" },
        );
        let _ = err.flush();
    }
}
