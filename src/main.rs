use std::net::UdpSocket;

use legato_hand_tracking::*;

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9870".into());
    let socket = UdpSocket::bind(&addr)?;

    eprintln!("listening on {addr}");

    // Largest frame we expect is a handful of hands; 4 KiB is plenty.
    let mut buf = [0u8; 4096];

    loop {
        let len = socket.recv(&mut buf)?;

        match Frame::decode(&buf[..len]) {
            Ok(frame) => {
                for hand in &frame.hands {
                    let [x, y, z] = hand.landmarks[Hand::INDEX_TIP];
                    println!(
                        "{:>10}us {:?} ({:.2}) index tip: {x:.3} {y:.3} {z:.3}",
                        frame.timestamp_us, hand.handedness, hand.score
                    );
                }
            }
            Err(e) => eprintln!("bad packet: {e}"),
        }
    }
}
