use std::{
    ops::{Range, RangeBounds},
    sync::{Arc, Mutex},
};

use windows::{
    core,
    Win32::Storage::CloudFilters::{CfReportProviderProgress, CF_CONNECTION_KEY},
};

use crate::{
    command::{self, Command, Fallible},
    error::CloudErrorKind,
    filter::{RawConnectionKey, RawRequestKey, RawTransferKey},
    metadata::Metadata,
    placeholder_create_info::PlaceholderCreateInfo,
    sealed, utility,
};

/// Optional changes applied when restarting hydration.
///
/// Defaults preserve metadata and identity. Restarting discards all resident data,
/// even when no changes are supplied. Only `HydrationType::Full` is supported.
#[derive(Debug, Clone, Default)]
pub struct RestartHydrationOptions<'a> {
    metadata: Option<Metadata>,
    blob: &'a [u8],
    mark_in_sync: bool,
}

impl<'a> RestartHydrationOptions<'a> {
    /// Replaces metadata during restart. Preserve the size from [`Request::file_size`][crate::filter::Request::file_size];
    /// size changes return `ERROR_NOT_SUPPORTED` because resizing pending hydration
    /// stalled on Windows 10 build 19044. Zero timestamps or attributes preserve those fields.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Replaces the file identity. An empty slice preserves the current identity.
    pub fn blob(mut self, blob: &'a [u8]) -> Self {
        self.blob = blob;
        self
    }

    /// Marks the placeholder in sync when restart succeeds.
    pub fn mark_in_sync(mut self) -> Self {
        self.mark_in_sync = true;
        self
    }
}

/// A ticket for the [SyncFilter::fetch_data][crate::filter::SyncFilter::fetch_data] callback.
#[derive(Debug)]
pub struct FetchData {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
    file_size: u64,
    /// The requested range to include in a failed transfer response.
    range: Range<u64>,
}

impl FetchData {
    /// Create a new [FetchData].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        file_size: u64,
        range: Range<u64>,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
            file_size,
            range,
        }
    }

    /// Fails the fetch with `error_kind`: the program reading the file gets that error rather
    /// than waiting for the platform's timeout. Fails if the fetch was already cancelled or
    /// timed out.
    pub fn fail(&self, error_kind: CloudErrorKind) -> core::Result<()> {
        command::Write::fail(
            self.connection_key,
            self.transfer_key,
            self.request_key,
            self.range.clone(),
            error_kind,
        )
    }

    /// Reports provider progress while servicing a hydration request.
    /// This is advisory: it neither transfers data nor completes the fetch, and the
    /// UI shown depends on how hydration was initiated.
    /// Successful reports reset the pending fetch's timeout while the provider works.
    pub fn report_progress(&self, total: u64, completed: u64) -> core::Result<()> {
        unsafe {
            CfReportProviderProgress(
                CF_CONNECTION_KEY(self.connection_key),
                self.transfer_key,
                total as i64,
                completed as i64,
            )
        }?;

        Ok(())
    }

    /// Restarts hydration after a remote version change or invalid downloaded data.
    ///
    /// Requires `HydrationType::Full`. On success, stop servicing this callback:
    /// Windows discards resident data and reissues pending I/O as new callbacks.
    /// This does not replace normal placeholder updates or resolve version conflicts.
    /// If restart fails, return an error from the callback or explicitly fail the
    /// fetch; a rejected restart need not complete the pending read.
    pub fn restart_hydration(&self, options: RestartHydrationOptions<'_>) -> core::Result<()> {
        command::RestartHydration {
            file_size: self.file_size,
            metadata: options.metadata,
            blob: options.blob,
            mark_in_sync: options.mark_in_sync,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

impl utility::ReadAt for FetchData {
    /// Read data at an offset from a placeholder file.
    ///
    /// This method is equivalent to calling `CfExecute` with `CF_OPERATION_TYPE_RETRIEVE_DATA`.
    /// Requires `HydrationPolicy::ValidationRequired` and previously transferred data;
    /// it does not fetch missing bytes from the remote store.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> core::Result<u64> {
        command::Read {
            buffer: buf,
            position: offset,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

impl utility::WriteAt for FetchData {
    /// Write data at an offset to a placeholder file.
    ///
    /// The offset must be 4 KiB aligned. The length must be a multiple of 4 KiB,
    /// unless the range ends on or beyond the logical file size.
    ///
    /// This method is equivalent to calling `CfExecute` with `CF_OPERATION_TYPE_TRANSFER_DATA`.
    fn write_at(&self, buf: &[u8], offset: u64) -> core::Result<()> {
        command::Write {
            buffer: buf,
            position: offset,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

impl sealed::Sealed for FetchData {}

/// A ticket for the [SyncFilter::validate_data][crate::filter::SyncFilter::validate_data] callback.
#[derive(Debug)]
pub struct ValidateData {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
    file_size: u64,
}

impl ValidateData {
    /// Create a new [ValidateData].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        file_size: u64,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
            file_size,
        }
    }

    /// Acknowledges that the provider has validated the specified byte range.
    ///
    /// The offset must be 4 KiB aligned. The length must be a multiple of 4 KiB,
    /// unless the range ends on or beyond the logical file size. An unbounded end
    /// acknowledges through EOF.
    ///
    /// This method is equivalent to calling `CfExecute` with `CF_OPERATION_TYPE_ACK_DATA`.
    pub fn pass(&self, range: impl RangeBounds<u64>) -> core::Result<()> {
        command::Validate {
            range: (range.start_bound().cloned(), range.end_bound().cloned()),
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }

    /// Discards invalid data and restarts pending hydration instead of acknowledging it.
    /// See [`FetchData::restart_hydration`] for the policy and callback requirements.
    pub fn restart_hydration(&self, options: RestartHydrationOptions<'_>) -> core::Result<()> {
        command::RestartHydration {
            file_size: self.file_size,
            metadata: options.metadata,
            blob: options.blob,
            mark_in_sync: options.mark_in_sync,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

impl utility::ReadAt for ValidateData {
    /// Read data at an offset from a placeholder file.
    ///
    /// This method is equivalent to calling `CfExecute` with `CF_OPERATION_TYPE_RETRIEVE_DATA`.
    ///
    /// Requires `HydrationPolicy::ValidationRequired` and previously transferred data.
    /// Returns the number of bytes retrieved by the platform.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> core::Result<u64> {
        command::Read {
            buffer: buf,
            position: offset,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

impl sealed::Sealed for ValidateData {}

/// A ticket for the [SyncFilter::fetch_placeholders][crate::filter::SyncFilter::fetch_placeholders] callback.
#[derive(Debug)]
pub struct FetchPlaceholders {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
    count: Arc<Mutex<u64>>,
}

impl FetchPlaceholders {
    /// Create a new [FetchPlaceholders].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        count: Arc<Mutex<u64>>,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
            count,
        }
    }

    /// Creates one batch of placeholders without completing directory population.
    ///
    /// Attempts every entry and returns the first error, if any. Each entry's
    /// [PlaceholderCreateInfo::result] contains its creation result and USN.
    /// An empty slice is a no-op. Call this repeatedly for paginated listings and
    /// propagate errors from the callback. The library completes population when
    /// `fetch_placeholders` returns `Ok(())`, even if no batches were submitted.
    /// Finish all submissions before returning; do not retain the ticket for later work.
    pub fn pass_with_placeholders(
        &self,
        placeholders: &mut [PlaceholderCreateInfo],
    ) -> core::Result<()> {
        // Serialize batches so native totals stay ahead of every submitted entry,
        // including when the same ticket is shared between threads.
        let mut count = self.count.lock().unwrap();
        let total = count
            .checked_add(placeholders.len() as u64)
            .and_then(|count| count.checked_add(1))
            .filter(|&total| total <= i64::MAX as u64)
            .ok_or_else(|| core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
        *count = total - 1;
        command::CreatePlaceholders {
            total,
            placeholders,
        }
        .execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

/// A ticket for the [SyncFilter::dehydrate][crate::filter::SyncFilter::dehydrate] callback.
#[derive(Debug)]
pub struct Dehydrate {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
}

impl Dehydrate {
    /// Create a new [Dehydrate].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
        }
    }

    /// Allows the pending dehydration to proceed; success is not a completion notification.
    pub fn pass(&self) -> core::Result<()> {
        command::Dehydrate { blob: &[] }.execute(
            self.connection_key,
            self.transfer_key,
            self.request_key,
        )
    }

    /// Allows the pending dehydration and supplies a new file blob to the platform.
    pub fn pass_with_blob(&self, blob: &[u8]) -> core::Result<()> {
        command::Dehydrate { blob }.execute(
            self.connection_key,
            self.transfer_key,
            self.request_key,
        )
    }
}

/// A ticket for the [SyncFilter::delete][crate::filter::SyncFilter::delete] callback.
#[derive(Debug)]
pub struct Delete {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
}

impl Delete {
    /// Create a new [Delete].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
        }
    }

    /// Allows the pending deletion or undelete to proceed.
    /// This does not prove that the file has been permanently removed.
    pub fn pass(&self) -> core::Result<()> {
        command::Delete.execute(self.connection_key, self.transfer_key, self.request_key)
    }
}

/// A ticket for the [SyncFilter::rename][crate::filter::SyncFilter::rename] callback.
#[derive(Debug)]
pub struct Rename {
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
}

impl Rename {
    /// Create a new [Rename].
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
    ) -> Self {
        Self {
            connection_key,
            transfer_key,
            request_key,
        }
    }

    /// Allows the pending rename/move to proceed. The local operation can still fail.
    pub fn pass(&self) -> core::Result<()> {
        command::Rename.execute(self.connection_key, self.transfer_key, self.request_key)
    }
}
