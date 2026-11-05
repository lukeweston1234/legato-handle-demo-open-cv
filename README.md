This is a Legato demo that utilizes the `handle` functionality.

This enables users to easily make nodes that require thread synchronization primitives.

For instance, your factory function can register a producer or consumer, and you can later
use this in your LegatoFrontend.

This repository was pretty much vibe coded and extended from other OpenCV examples.

## Marionette

A small generative band, conducted with your hands (see `marionette.legato`).

Five voices (bass, two pads, a chime and an arp) pick their own notes from random
sources, and a custom `harmony` node snaps them onto a chord. With nobody in front of
the camera it drifts around A minor pentatonic on its own.

- **Left hand chooses the harmony.** Curl a finger to play its chord, each through
  its 9th: index Am9, middle Fmaj9, ring Cmaj9, pinky G9. Several fingers sound
  together, and the arp comes in while you play. Tuck your thumb to latch the chord
  and rest your hand.
- **Right hand conducts.** Raise it to go from a sparse drift to every voice on every
  step, pinch open to brighten, roll your wrist to lengthen the arp's attack, move
  side to side for its decay, lean in for reverb, and make a fist to hush everyone
  but the bass.

The camera preview shows the chord and the right hand's controls; the audio process
sends that back over the same UDP socket. There is no window on the Rust side, so the
audio thread has the box to itself.

To run:

```sh
uv run legato-hand-tracking & trap "kill $!" EXIT; cargo run --release
```

To hear the patch without a camera or audio interface:

```sh
cargo run --release --example render -- out.wav
```
