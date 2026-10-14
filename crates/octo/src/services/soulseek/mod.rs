//! `Services/Soulseek`: the outside-song machinery with I/O or shared state (the external id
//! registry, the metadata service, the radio queue and the download service). The pure parts
//! (the routing model, song lengths) are in `octo_core::soulseek`.

pub mod external_id_registry;
pub mod radio_queue_store;
pub mod soulseek_download_service;
pub mod soulseek_metadata_service;

pub use external_id_registry::{ExternalIdRegistry, SharedRouting};
pub use radio_queue_store::RadioQueueStore;
pub use soulseek_metadata_service::{LastFmTrackLengths, SoulseekMetadataService};
