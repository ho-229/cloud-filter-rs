use super::StateWatchEvent;

use crate::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request},
};

/// Core functions for implementing a Sync Engine.
///
/// [Send] and [Sync] are required as the callback could be invoked from an arbitrary thread, [read
/// here](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_callback_type#remarks).
///
/// These are Cloud Filter requests and notifications, not a filesystem change log.
/// The bridge registers every native callback, including methods with default
/// implementations. Returning `NotSupported` rejects a request; it does not opt out
/// of registering that callback. Successful request handlers must complete the
/// operation through their ticket, except `fetch_placeholders`, whose successful
/// return completes directory population after all submitted batches.
///
/// Provider-originated operations can bypass notifications. Test user actions from
/// a separate process; `Request::process` may return `None` for cancellation callbacks.
/// Local writes need separate change tracking;
/// `state_changed` is implemented by this crate's directory watcher.
pub trait SyncFilter: Send + Sync {
    /// A placeholder hydration has been requested. This means that the placeholder should be
    /// filled with the requested remote byte ranges. Cached reads need no fetch.
    fn fetch_data(
        &self,
        _request: Request,
        _ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> CResult<()>;

    /// A pending byte range is no longer needed. This may cover only part of a fetch.
    /// Inspect the cancellation flags; it is not a notification for every closed handle.
    fn cancel_fetch_data(&self, _request: Request, _info: info::CancelFetchData) {}

    /// Verify transferred data before the platform completes reads of that range.
    /// Validation can run while other ranges are still being fetched.
    ///
    /// **You** are responsible for validating the data in the placeholder. To approve
    /// the request, use the ticket provided.
    ///
    /// Note that this callback is only called if [HydrationPolicy::ValidationRequired][crate::root::HydrationPolicy::ValidationRequired]
    /// is specified.
    fn validate_data(
        &self,
        _request: Request,
        _ticket: ticket::ValidateData,
        _info: info::ValidateData,
    ) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }

    /// A directory population has been requested. The behavior of this callback is dependent on
    /// the [PopulationType][crate::root::PopulationType] variant specified during registration.
    /// Queries or opens below a partially populated placeholder directory can trigger it.
    /// Ordinary directories and fully populated directories do not need this callback.
    /// `AlwaysFull` population disables it entirely. Completion is not a remote refresh
    /// subscription: the provider must discover later remote changes itself.
    /// With `PopulationType::Full`, supply all remaining children, even when the
    /// search pattern matches only a subset.
    ///
    /// Submit any number of batches through the ticket before returning. `Ok(())`
    /// completes population and disables further on-demand enumeration; returning
    /// `Err` fails the request and leaves it incomplete. Success without batches
    /// also completes population. Finish all ticket work before this callback returns.
    fn fetch_placeholders(
        &self,
        _request: Request,
        _ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }

    /// A directory population request has been cancelled.
    fn cancel_fetch_placeholders(&self, _request: Request, _info: info::CancelFetchPlaceholders) {}

    /// A placeholder file handle has been opened for read, write, and/or delete
    /// access. Attribute-only opens and directories are excluded. No acknowledgment is needed.
    fn opened(&self, _request: Request, _info: info::Opened) {}

    /// A placeholder file handle that has been previously opened with read, write,
    /// and/or delete access has been closed. Inspect `info.deleted()` for deletion
    /// on close. Directories do not receive this notification.
    fn closed(&self, _request: Request, _info: info::Closed) {}

    /// The platform requests permission to dehydrate a placeholder.
    ///
    /// The requesting application is blocked until the ticket is acknowledged or failed.
    /// Pin/unpin attribute changes alone do not guarantee this callback. Provider-issued
    /// dehydration can bypass it, so enforce provider-side checks at that call site too.
    /// Acknowledgment permits dehydration; it does not prove that it has completed.
    fn dehydrate(
        &self,
        _request: Request,
        _ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }

    /// A placeholder dehydration has completed successfully. No acknowledgment is needed.
    fn dehydrated(&self, _request: Request, _info: info::Dehydrated) {}

    /// A placeholder file or directory is about to be deleted (or undeleted; inspect the flags).
    ///
    /// The operating system will handle deleting placeholder files automatically. However, it is
    /// up to **you** to approve this. Use the ticket to approve the request.
    /// An application can later undo its delete disposition. Do not treat approval
    /// as permission to irreversibly discard the only remote copy.
    fn delete(
        &self,
        _request: Request,
        _ticket: ticket::Delete,
        _info: info::Delete,
    ) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }

    /// A local delete operation completed. No acknowledgment is needed.
    /// This can precede an undelete while a handle remains open. Recheck the final
    /// local state before permanently deleting remote data.
    fn deleted(&self, _request: Request, _info: info::Deleted) {}

    /// A placeholder file or directory is about to be renamed or moved. Inspect both scope flags.
    ///
    /// The operating system will handle moving and renaming placeholder files automatically.
    /// However, it is up to **you** to approve this. Use the ticket to approve the
    /// request.
    ///
    /// A successful operation can produce the [SyncFilter::renamed] completion callback.
    /// Acknowledgment permits the local operation; it is not itself proof of completion.
    fn rename(
        &self,
        _request: Request,
        _ticket: ticket::Rename,
        _info: info::Rename,
    ) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }

    /// A placeholder file or directory has been renamed or moved. No acknowledgment is needed.
    fn renamed(&self, _request: Request, _info: info::Renamed) {}

    /// Reports paths to recheck after name, content, size, or attribute changes.
    ///
    /// This crate uses [ReadDirectoryChangesW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw),
    /// independently of CF callbacks, to detect local writes and pin/unpin changes.
    /// Lost notifications trigger a root scan delivered in [StateWatchEvent::Changed] batches.
    /// Paths may repeat, be unchanged, or no longer exist; inspect their current state.
    /// These notifications are not a durable journal.
    ///
    /// Runs on the watcher thread, so queue slow work elsewhere. [StateWatchEvent::Stopped]
    /// terminates the watcher; normal disconnect emits no error.
    fn state_changed(&self, _event: StateWatchEvent) {}
}
