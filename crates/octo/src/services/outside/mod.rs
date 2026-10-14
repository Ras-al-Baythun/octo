//! `Services/Outside`: the outside-song machinery with I/O or shared state (the external id
//! registry, the metadata service, the radio queue and the download service). The pure parts
//! (the routing model, song lengths) are in `octo_core::outside`.

pub mod external_id_registry;
pub mod outside_download_service;
pub mod outside_metadata_service;
pub mod radio_queue_store;

pub use external_id_registry::{ExternalIdRegistry, SharedRouting};
pub use outside_metadata_service::{LastFmTrackLengths, OutsideMetadataService};
pub use radio_queue_store::RadioQueueStore;
