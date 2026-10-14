//! The outside-song machinery: the routing model, the shown-length rules and the stream facts.
//! The registry and services that do I/O live in `octo::services::soulseek`.

pub mod song_length;
pub mod soulseek_metadata_service;
pub mod stream_facts;

pub use song_length::{LengthSource, SongLength};
pub use soulseek_metadata_service::{RoutingKind, SoulseekRouting};
pub use stream_facts::{DeclaredStream, StreamFacts};
