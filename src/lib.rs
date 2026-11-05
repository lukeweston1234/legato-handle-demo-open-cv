//! Marionette: a generative patch conducted through a webcam.
//!
//! `tracking.py` sends hand landmarks over UDP ([`protocol`]), the network
//! thread turns them into a [`gesture::Gesture`], and the [`nodes::Hands`]
//! node hands that to the audio graph. In `marionette.legato`, random sources
//! play a handful of voices, the [`nodes::Harmony`] node snaps them onto the
//! chords the playing hand holds, and the other hand conducts density and
//! timbre. The network thread sends a [`status::Status`] back for the
//! tracker's overlay.

pub mod gesture;
pub mod nodes;
pub mod protocol;
pub mod status;

use legato::prelude::*;

pub use protocol::{Frame, Hand, Handedness, LANDMARKS};

/// Build a graph with Marionette's nodes registered under `marionette { }`.
pub fn build(config: Config, graph: &str) -> Result<(LegatoApp, LegatoFrontend), ValidationError> {
    LegatoBuilder::<Unconfigured>::new(config)
        .register_node("marionette", nodes::Hands::spec())
        .register_node("marionette", nodes::Harmony::spec())
        .register_node("marionette", nodes::Pluck::spec())
        .build_dsl(graph)
}
