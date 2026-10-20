This is a Legato demo that utilizes the `handle` functionality.

This enables users to easily make nodes that require thread synchronization primitives.

For instance, your factory function can register a producer or consumer, and you can later
use this in your LegatoFrontend.

This repository was pretty much vibe coded and extended from other OpenCV examples.

I find it the most fun to use the keyboard to trigger samples and automate them with your right hand,
although you can use the left hand to trigger samples as well, it's just not very sharp IMO and kind 
of unergonomic compared to a keyboard.

To run:

```sh
uv run tracking.py & cargo run --release <your-sample>
```
