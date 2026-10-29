use std::{future::Future, ops::Deref};

use crate::{
    error::{CResult, CloudErrorKind},
    filter::Request,
    utility::LocalBoxFuture,
};

use super::{info, ticket, StateWatchEvent, SyncFilter};

/// Async core functions for implementing a Sync Engine.
///
/// [Send] and [Sync] are required as the callback could be invoked from an arbitrary thread, [read
/// here](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_callback_type#remarks).
///
/// These are Cloud Filter requests and notifications, not a filesystem change log.
/// The bridge registers every native callback, including methods with default
/// implementations. Returning `NotSupported` rejects a request; it does not opt out
/// of registering that callback. Successful request handlers must complete the
/// operation through their ticket; returning `Ok(())` alone does not acknowledge it.
///
/// Provider-originated operations can bypass notifications. Test user actions from
/// a separate process, and use `Request::try_process` because cancellation callbacks
/// may have no process information. Local writes need separate change tracking;
/// `state_changed` is implemented by this crate's directory watcher.
pub trait Filter: Send + Sync {
    /// A placeholder hydration has been requested. This means that the placeholder should be
    /// filled with the requested remote byte ranges. Cached reads need no fetch.
    fn fetch_data(
        &self,
        _request: Request,
        _ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> impl Future<Output = CResult<()>>;

    /// A pending byte range is no longer needed. This may cover only part of a fetch.
    /// Inspect the cancellation flags; it is not a notification for every closed handle.
    fn cancel_fetch_data(
        &self,
        _request: Request,
        _info: info::CancelFetchData,
    ) -> impl Future<Output = ()> {
        async {}
    }

    /// Followed by a successful call to [Filter::fetch_data][super::Filter::fetch_data], this callback should verify the integrity of
    /// the data persisted in the placeholder.
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
    ) -> impl Future<Output = CResult<()>> {
        async { Err(CloudErrorKind::NotSupported) }
    }

    /// A directory population has been requested. The behavior of this callback is dependent on
    /// the [PopulationType][crate::root::PopulationType] variant specified during registration.
    /// Queries or opens below a partially populated placeholder directory can trigger it.
    /// Ordinary directories and fully populated directories do not need this callback.
    /// `AlwaysFull` population disables it entirely. Completion is not a remote refresh
    /// subscription: the provider must discover later remote changes itself.
    fn fetch_placeholders(
        &self,
        _request: Request,
        _ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> impl Future<Output = CResult<()>> {
        async { Err(CloudErrorKind::NotSupported) }
    }

    /// A directory population request has been cancelled.
    fn cancel_fetch_placeholders(
        &self,
        _request: Request,
        _info: info::CancelFetchPlaceholders,
    ) -> impl Future<Output = ()> {
        async {}
    }

    /// A placeholder file handle has been opened for read, write, and/or delete
    /// access. Attribute-only opens and directories are excluded. No acknowledgment is needed.
    fn opened(&self, _request: Request, _info: info::Opened) -> impl Future<Output = ()> {
        async {}
    }

    /// A placeholder file handle that has been previously opened with read, write,
    /// and/or delete access has been closed.
    fn closed(&self, _request: Request, _info: info::Closed) -> impl Future<Output = ()> {
        async {}
    }

    /// The platform requests permission to dehydrate a placeholder.
    ///
    /// The requesting application is blocked until the ticket is acknowledged or failed.
    /// Pin/unpin attribute changes alone do not guarantee this callback. Provider-issued
    /// dehydration can bypass it, so enforce provider-side checks at that call site too.
    fn dehydrate(
        &self,
        _request: Request,
        _ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> impl Future<Output = CResult<()>> {
        async { Err(CloudErrorKind::NotSupported) }
    }

    /// A placeholder dehydration has completed successfully. No acknowledgment is needed.
    fn dehydrated(&self, _request: Request, _info: info::Dehydrated) -> impl Future<Output = ()> {
        async {}
    }

    /// A placeholder file or directory is about to be deleted (or undeleted; inspect the flags).
    ///
    /// The operating system will handle deleting placeholder files automatically. However, it is
    /// up to **you** to approve this. Use the ticket to approve the request.
    fn delete(
        &self,
        _request: Request,
        _ticket: ticket::Delete,
        _info: info::Delete,
    ) -> impl Future<Output = CResult<()>> {
        async { Err(CloudErrorKind::NotSupported) }
    }

    /// A placeholder file or directory has been deleted. No acknowledgment is needed.
    fn deleted(&self, _request: Request, _info: info::Deleted) -> impl Future<Output = ()> {
        async {}
    }

    /// A placeholder file or directory is about to be renamed or moved. Inspect both scope flags.
    ///
    /// The operating system will handle moving and renaming placeholder files automatically.
    /// However, it is up to **you** to approve this. Use the ticket to approve the
    /// request.
    ///
    /// A successful operation can produce the [Filter::renamed] completion callback.
    /// Acknowledgment permits the local operation; it is not itself proof of completion.
    fn rename(
        &self,
        _request: Request,
        _ticket: ticket::Rename,
        _info: info::Rename,
    ) -> impl Future<Output = CResult<()>> {
        async { Err(CloudErrorKind::NotSupported) }
    }

    /// A placeholder file or directory has been renamed or moved. No acknowledgment is needed.
    fn renamed(&self, _request: Request, _info: info::Renamed) -> impl Future<Output = ()> {
        async {}
    }

    /// Paths to recheck after name, content, size, or attribute changes, or watcher termination.
    ///
    /// Lost notifications trigger an internal root scan, reported incrementally through
    /// [StateWatchEvent::Changed]. Recheck current state; paths may repeat or be unchanged.
    /// `Stopped` is terminal for the watcher. Normal disconnect emits no error.
    /// This callback runs on the watcher thread: queue slow work elsewhere.
    ///
    /// This callback is implemented using [ReadDirectoryChangesW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw)
    /// so it is not provided by the `Cloud Filter APIs`.
    ///
    /// Use this to detect local writes and newly created files as well as pin/unpin changes.
    /// Paths may repeat or no longer exist; inspect their current state. Queue slow work
    /// rather than blocking the directory watcher. Notifications are not a durable journal.
    ///
    /// See also [Cloud Files API Frequently Asked Questions](https://www.userfilesystem.com/programming/faq/).
    fn state_changed(&self, _event: StateWatchEvent) -> impl Future<Output = ()> {
        async {}
    }
}

/// Adapts a [Filter] to the [SyncFilter] trait.
pub struct AsyncBridge<F, B> {
    filter: F,
    block_on: B,
}

impl<F, B> AsyncBridge<F, B>
where
    F: Filter,
    B: Fn(LocalBoxFuture<'_, ()>) + Send + Sync,
{
    pub(crate) fn new(filter: F, block_on: B) -> Self {
        Self { filter, block_on }
    }
}

impl<F, B> SyncFilter for AsyncBridge<F, B>
where
    F: Filter,
    B: Fn(LocalBoxFuture<'_, ()>) + Send + Sync,
{
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.fetch_data(request, ticket, info).await;
        }));

        ret
    }

    fn cancel_fetch_data(&self, request: Request, info: info::CancelFetchData) {
        (self.block_on)(Box::pin(self.filter.cancel_fetch_data(request, info)))
    }

    fn validate_data(
        &self,
        request: Request,
        ticket: ticket::ValidateData,
        info: info::ValidateData,
    ) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.validate_data(request, ticket, info).await;
        }));

        ret
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        info: info::FetchPlaceholders,
    ) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.fetch_placeholders(request, ticket, info).await;
        }));

        ret
    }

    fn cancel_fetch_placeholders(&self, request: Request, info: info::CancelFetchPlaceholders) {
        (self.block_on)(Box::pin(
            self.filter.cancel_fetch_placeholders(request, info),
        ))
    }

    fn opened(&self, request: Request, info: info::Opened) {
        (self.block_on)(Box::pin(self.filter.opened(request, info)))
    }

    fn closed(&self, request: Request, info: info::Closed) {
        (self.block_on)(Box::pin(self.filter.closed(request, info)))
    }

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        info: info::Dehydrate,
    ) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.dehydrate(request, ticket, info).await;
        }));

        ret
    }

    fn dehydrated(&self, request: Request, info: info::Dehydrated) {
        (self.block_on)(Box::pin(self.filter.dehydrated(request, info)))
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.delete(request, ticket, info).await;
        }));

        ret
    }

    fn deleted(&self, request: Request, info: info::Deleted) {
        (self.block_on)(Box::pin(self.filter.deleted(request, info)))
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        let mut ret = Err(CloudErrorKind::Unsuccessful);
        (self.block_on)(Box::pin(async {
            ret = self.filter.rename(request, ticket, info).await;
        }));

        ret
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        (self.block_on)(Box::pin(self.filter.renamed(request, info)))
    }

    fn state_changed(&self, event: StateWatchEvent) {
        (self.block_on)(Box::pin(self.filter.state_changed(event)))
    }
}

impl<F, B> Deref for AsyncBridge<F, B>
where
    F: Filter,
    B: Fn(LocalBoxFuture<'_, ()>) + Send + Sync,
{
    type Target = F;

    fn deref(&self) -> &Self::Target {
        &self.filter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO;

    struct PendingFilter;
    impl Filter for PendingFilter {
        async fn fetch_data(
            &self,
            _: Request,
            _: ticket::FetchData,
            _: info::FetchData,
        ) -> CResult<()> {
            std::future::pending().await
        }
    }

    #[test]
    fn unfinished_executor_cannot_report_success() {
        // The caller supplies block_on as a safe closure; it can simply discard the future.
        let bridge = AsyncBridge::new(PendingFilter, |_: LocalBoxFuture<'_, ()>| {});
        let result = SyncFilter::fetch_data(
            &bridge,
            unsafe { Request::new(CF_CALLBACK_INFO::default()) },
            ticket::FetchData::new(0, 0, 0..1),
            info::FetchData(Default::default()),
        );
        assert!(matches!(result, Err(CloudErrorKind::Unsuccessful)));
    }
}
