//! YouTube (InnerTube) access.

pub mod client;
pub mod model;
pub mod parse;
pub mod resolver;
pub mod session;

pub use client::Client;
pub use resolver::{Resolver, StreamInfo};
pub use session::Session;
