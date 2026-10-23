//! Audio: HTTP stream → demux → decode → resample → ring → EQ → device.

pub mod engine;
pub mod eq;
pub mod output;
pub mod resample;
pub mod source;
pub mod stream;
pub mod webm;

pub use engine::{Engine, Event, Position};
pub use eq::EqSettings;
