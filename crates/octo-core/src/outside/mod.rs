//! The outside-song machinery: the routing model, the shown-length rules and the stream facts.
//! The registry and services that do I/O live in `octo::services::outside`.

pub mod routing;
pub mod song_length;
pub mod stream_facts;

pub use routing::{OutsideRouting, RoutingKind};
pub use song_length::{LengthSource, SongLength};
pub use stream_facts::{DeclaredStream, StreamFacts};
