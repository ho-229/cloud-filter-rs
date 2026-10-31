#![doc = include_str!("../README.md")]

/// Contains callbacks error types.
pub mod error;
/// Contains traits extending common structs from the [std].
pub mod ext;
/// Contains the [SyncFilter][crate::filter::SyncFilter] and [Filter][crate::filter::Filter] traits
/// and related structs.
pub mod filter;
/// Contains the [Metadata][crate::metadata::Metadata] struct.
pub mod metadata;
/// Contains the [Placeholder][crate::placeholder::Placeholder] struct.
pub mod placeholder;
/// Contains the [PlaceholderCreateInfo][crate::placeholder_create_info::PlaceholderCreateInfo] struct.
pub mod placeholder_create_info;
/// Contains [properties::set_item_properties], the provider's own states Explorer shows.
pub mod properties;
/// Contains the sync root structs.
pub mod root;
pub mod usn;
pub mod utility;

/// Contains low-level structs for directly executing Cloud Filter operations.
///
/// The [command][crate::command] API is exposed through various higher-level structs, like
/// [Request][crate::request::Request] and [Placeholder][crate::placeholder::Placeholder].
mod command;

mod sealed {
    pub trait Sealed {}
}
