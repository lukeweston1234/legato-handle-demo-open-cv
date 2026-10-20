//! Marionette: a two-handed slicing instrument played through a webcam.
//!
//! `tracking.py` sends hand landmarks over UDP ([`protocol`]), the network
//! thread turns them into a [`gesture::Gesture`], and the [`nodes::Hands`]
//! node hands that to the audio graph, where `marionette.legato` patches it
//! into the [`nodes::Slicer`]. The [`viewer`] shows the slicer's sample and
//! edits its [`bounds`] live.

pub mod bounds;
pub mod gesture;
pub mod nodes;
pub mod protocol;
pub mod viewer;

use legato::prelude::*;

pub use protocol::{Frame, Hand, Handedness, LANDMARKS};

/// Build a graph with Marionette's nodes registered under `marionette { }`.
pub fn build(config: Config, graph: &str) -> Result<(LegatoApp, LegatoFrontend), ValidationError> {
    LegatoBuilder::<Unconfigured>::new(config)
        .register_node("marionette", nodes::Hands::spec())
        .register_node("marionette", nodes::Slicer::spec())
        .build_dsl(graph)
}
