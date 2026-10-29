/// This module contains the structs that are used to pass information into the callbacks of the
/// [SyncFilter][crate::filter::SyncFilter] trait and the [Filter][crate::filter::Filter] trait.
pub mod info;

/// This module contains the structs that are used to represent the operation that the engine
/// needs to perform. They are used as parameters to the methods of the [SyncFilter][crate::filter::SyncFilter]
/// trait and the [Filter][crate::filter::Filter] trait.
pub mod ticket;

pub use async_filter::{AsyncBridge, Filter};
pub(crate) use proxy::{callbacks, Callbacks};
pub use request::{Process, Request};
pub(crate) use request::{RawConnectionKey, RawTransferKey};
pub use sync_filter::SyncFilter;

mod async_filter;
mod proxy;
mod request;
mod sync_filter;

/// Notifications from the sync root's attribute watcher, not native CF callbacks.
#[derive(Debug)]
pub enum StateWatchEvent {
    /// Paths whose current attributes should be checked. Paths may repeat or no longer exist.
    /// If notifications are lost, the library scans the root and reports descendants in
    /// batches of at most 64 paths, including paths whose attributes may not have changed.
    /// The scan does not follow symlinks or junctions. Scan errors terminate the watcher.
    Changed(Vec<std::path::PathBuf>),
    /// The watcher terminated after a runtime error. No further events follow.
    /// The CF connection remains active; reconnect to start another watcher.
    Stopped(windows::core::Error),
}
