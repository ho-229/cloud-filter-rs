/// Callback parameters for [SyncFilter] and [Filter].
pub mod info;

/// Tickets for supplying data and responding to [SyncFilter] and [Filter] requests.
pub mod ticket;

pub use async_filter::{AsyncBridge, Filter};
pub(crate) use proxy::{callbacks, Callbacks};
pub use request::{Process, Request};
pub(crate) use request::{RawConnectionKey, RawRequestKey, RawTransferKey};
pub use sync_filter::SyncFilter;

mod async_filter;
mod proxy;
mod request;
mod sync_filter;

/// Notifications from the sync root's directory watcher, not native CF callbacks.
#[derive(Debug)]
pub enum StateWatchEvent {
    /// Paths whose current state should be checked. Paths may repeat or no longer exist.
    /// If notifications are lost, the library scans the root and reports descendants in
    /// batches of at most 64 paths, including paths whose state may not have changed.
    /// The scan does not follow symlinks or junctions. Scan errors terminate the watcher.
    Changed(Vec<std::path::PathBuf>),
    /// The watcher terminated after a runtime error. No further events follow.
    /// The CF connection remains active; reconnect to start another watcher.
    Stopped(windows::core::Error),
}
