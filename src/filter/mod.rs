/// Information for callbacks in the [SyncFilter][crate::SyncFilter] trait.
pub mod info;
/// Tickets for callbacks in the [SyncFilter][crate::SyncFilter] trait.
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
