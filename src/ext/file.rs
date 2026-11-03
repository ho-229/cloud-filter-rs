use std::{
    fs::File,
    mem,
    ops::RangeBounds,
    os::windows::{io::AsRawHandle, prelude::RawHandle},
};

use flagset::FlagSet;
use widestring::{U16CStr, U16CString};
use windows::{
    core,
    Win32::{
        Foundation::{
            ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT, ERROR_INVALID_DATA, ERROR_MORE_DATA,
            ERROR_NOT_SUPPORTED, HANDLE,
        },
        Storage::CloudFilters::{
            self, CfDehydratePlaceholder, CfGetSyncRootInfoByHandle, CF_SYNC_PROVIDER_STATUS,
            CF_SYNC_ROOT_BASIC_INFO, CF_SYNC_ROOT_STANDARD_INFO,
        },
    },
};

use crate::{
    root::{HydrationPolicy, HydrationType, PopulationType, SupportedAttribute},
    sealed::Sealed,
    usn, utility,
};

/// An API extension to [File][std::fs::File].
pub trait FileExt: AsRawHandle + Sealed {
    /// Reads the latest journaled USN through a file or directory handle.
    /// This waits for completion, including on handles opened for overlapped I/O.
    /// This queries NTFS V2 records without reading file contents or hydrating data.
    /// Unsupported filesystems and device errors are returned unchanged.
    ///
    /// A USN comparison does not lock the file; callers must still exclude writes
    /// while publishing data and marking it in sync.
    fn usn(&self) -> core::Result<usn::Usn> {
        usn::read(self.as_raw_handle())
    }

    /// Dehydrates a placeholder file.
    fn dehydrate<T: RangeBounds<u64>>(&self, range: T) -> core::Result<()> {
        dehydrate(self.as_raw_handle(), range, false)
    }

    /// Dehydrates a placeholder file as a system process running in the background. Otherwise, it
    /// is called on behalf of a logged-in user.
    fn background_dehydrate<T: RangeBounds<u64>>(&self, range: T) -> core::Result<()> {
        dehydrate(self.as_raw_handle(), range, true)
    }

    /// Returns whether this file or directory belongs to a registered sync root,
    /// including the root itself and ordinary files that are not placeholders.
    ///
    /// Queries native registration without hydrating data or requiring a connected
    /// provider. The handle needs `FILE_READ_ATTRIBUTES` access. Other query errors
    /// are returned rather than treated as absence of a sync root.
    fn in_sync_root(&self) -> core::Result<bool> {
        let mut info = CF_SYNC_ROOT_BASIC_INFO::default();
        match unsafe {
            CfGetSyncRootInfoByHandle(
                HANDLE(self.as_raw_handle()),
                CloudFilters::CF_SYNC_ROOT_INFO_BASIC,
                (&mut info as *mut CF_SYNC_ROOT_BASIC_INFO).cast(),
                mem::size_of_val(&info) as u32,
                None,
            )
        } {
            Ok(()) => Ok(true),
            Err(error) if error.code() == ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT.to_hresult() => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Queries a snapshot of the containing sync root's native registration and provider status.
    ///
    /// Requires `FILE_READ_ATTRIBUTES` access and does not hydrate data. Returns `None`
    /// outside a registered sync root, including after unregistering it. Other errors propagate.
    /// Unlike [`crate::root::SyncRootInfo`], this describes the live CFAPI registration,
    /// without requiring a WinRT registration or a connected provider.
    /// Unrecognized primary hydration or population policies return `ERROR_NOT_SUPPORTED`.
    fn sync_root_info(&self) -> core::Result<Option<SyncRootInfo>> {
        let mut data = vec![0u8; mem::size_of::<CF_SYNC_ROOT_STANDARD_INFO>()];
        let mut length = 0;
        let query = |data: &mut [u8], length: &mut u32| unsafe {
            CfGetSyncRootInfoByHandle(
                HANDLE(self.as_raw_handle()),
                CloudFilters::CF_SYNC_ROOT_INFO_STANDARD,
                data.as_mut_ptr().cast(),
                data.len() as u32,
                Some(length),
            )
        };
        let mut result = query(&mut data, &mut length);
        if result
            .as_ref()
            .is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult())
        {
            // CF_SYNC_REGISTRATION limits the root identity to 64 KiB. A second
            // query with that capacity also tolerates an identity update between calls.
            data.resize(mem::size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + 64 * 1024, 0);
            result = query(&mut data, &mut length);
        }
        match result {
            Ok(()) => SyncRootInfo::new(&data, length as usize).map(Some),
            Err(error) if error.code() == ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT.to_hresult() => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

// TODO: is `CfDehydratePlaceholder` deprecated?
// https://docs.microsoft.com/en-us/answers/questions/723805/what-is-the-behavior-of-file-ranges-in-different-p.html
fn dehydrate<T: RangeBounds<u64>>(
    handle: RawHandle,
    range: T,
    background: bool,
) -> core::Result<()> {
    let (offset, length) = utility::checked_range(range)?;
    unsafe {
        CfDehydratePlaceholder(
            HANDLE(handle),
            offset,
            length,
            if background {
                CloudFilters::CF_DEHYDRATE_FLAG_BACKGROUND
            } else {
                CloudFilters::CF_DEHYDRATE_FLAG_NONE
            },
            None,
        )
    }
}

impl FileExt for File {}

impl Sealed for File {}

/// An owned snapshot of a native sync root, queried with [`FileExt::sync_root_info`].
#[derive(Debug, Clone)]
pub struct SyncRootInfo {
    file_id: u64,
    hydration_type: HydrationType,
    hydration_policy: FlagSet<HydrationPolicy>,
    population_type: PopulationType,
    supported_attribute: FlagSet<SupportedAttribute>,
    hardlinks_allowed: bool,
    status: ProviderStatus,
    provider_name: U16CString,
    version: U16CString,
    blob: Vec<u8>,
}

impl SyncRootInfo {
    fn new(data: &[u8], length: usize) -> core::Result<Self> {
        let invalid = || core::Error::from(ERROR_INVALID_DATA);
        let header = data
            .get(..mem::size_of::<CF_SYNC_ROOT_STANDARD_INFO>())
            .ok_or_else(invalid)?;
        // The byte buffer need not be aligned. Copy all fields into owned values;
        // neither the result nor its clones retain pointers into the native buffer.
        let info = unsafe {
            header
                .as_ptr()
                .cast::<CF_SYNC_ROOT_STANDARD_INFO>()
                .read_unaligned()
        };
        let start = mem::offset_of!(CF_SYNC_ROOT_STANDARD_INFO, SyncRootIdentity);
        let end = start
            .checked_add(info.SyncRootIdentityLength as usize)
            .ok_or_else(invalid)?;
        let blob = data
            .get(..length)
            .and_then(|data| data.get(start..end))
            .ok_or_else(invalid)?;
        Ok(Self {
            file_id: info.SyncRootFileId as u64,
            hydration_type: match info.HydrationPolicy.Primary {
                CloudFilters::CF_HYDRATION_POLICY_PARTIAL => HydrationType::Partial,
                CloudFilters::CF_HYDRATION_POLICY_PROGRESSIVE => HydrationType::Progressive,
                CloudFilters::CF_HYDRATION_POLICY_FULL => HydrationType::Full,
                CloudFilters::CF_HYDRATION_POLICY_ALWAYS_FULL => HydrationType::AlwaysFull,
                _ => return Err(ERROR_NOT_SUPPORTED.into()),
            },
            hydration_policy: FlagSet::new_truncated(info.HydrationPolicy.Modifier.0 as u32),
            population_type: match info.PopulationPolicy.Primary {
                CloudFilters::CF_POPULATION_POLICY_FULL => PopulationType::Full,
                CloudFilters::CF_POPULATION_POLICY_ALWAYS_FULL => PopulationType::AlwaysFull,
                _ => return Err(ERROR_NOT_SUPPORTED.into()),
            },
            supported_attribute: FlagSet::new_truncated(info.InSyncPolicy.0),
            hardlinks_allowed: info.HardLinkPolicy == CloudFilters::CF_HARDLINK_POLICY_ALLOWED,
            status: info.ProviderStatus.into(),
            provider_name: U16CStr::from_slice_truncate(&info.ProviderName)
                .map_err(|_| invalid())?
                .to_ucstring(),
            version: U16CStr::from_slice_truncate(&info.ProviderVersion)
                .map_err(|_| invalid())?
                .to_ucstring(),
            blob: blob.to_vec(),
        })
    }

    /// The file ID of the sync root.
    pub fn file_id(&self) -> u64 {
        self.file_id
    }

    /// The primary hydration policy.
    pub fn hydration_type(&self) -> HydrationType {
        self.hydration_type
    }

    /// The known hydration policy modifiers. Unrecognized bits are ignored.
    pub fn hydration_policy(&self) -> FlagSet<HydrationPolicy> {
        self.hydration_policy
    }

    /// The population policy.
    pub fn population_type(&self) -> PopulationType {
        self.population_type
    }

    /// The known in-sync policy flags. Unrecognized bits are ignored.
    pub fn supported_attribute(&self) -> FlagSet<SupportedAttribute> {
        self.supported_attribute
    }

    /// Whether or not hardlinks are allowed by the sync root.
    pub fn hardlinks_allowed(&self) -> bool {
        self.hardlinks_allowed
    }

    /// The status reported at query time. Native file-handle queries can briefly
    /// lag updates; use [`crate::root::Connection::status`] for the connection's status.
    pub fn status(&self) -> ProviderStatus {
        self.status
    }

    /// The name of the sync provider.
    pub fn provider_name(&self) -> &U16CStr {
        &self.provider_name
    }

    /// The version of the sync provider.
    pub fn version(&self) -> &U16CStr {
        &self.version
    }

    /// The register blob associated with the sync root.
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }
}

/// Sync provider status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    /// The sync provider is disconnected.
    Disconnected,
    /// The sync provider is idle.
    Idle,
    /// The sync provider is populating a namespace.
    PopulateNamespace,
    /// The sync provider is populating placeholder metadata.
    PopulateMetadata,
    /// The sync provider is populating placeholder content.
    PopulateContent,
    /// The sync provider is incrementally syncing placeholder content.
    SyncIncremental,
    /// The sync provider has fully synced placeholder data.
    SyncFull,
    /// The sync provider has lost connectivity.
    ConnectivityLost,
    /// The sync provider has been terminated.
    Terminated,
    /// The sync provider had an error.
    Error,
    /// An unrecognized status or a combination of native status flags.
    Unknown(u32),
}

impl From<CF_SYNC_PROVIDER_STATUS> for ProviderStatus {
    fn from(status: CF_SYNC_PROVIDER_STATUS) -> Self {
        match status {
            CloudFilters::CF_PROVIDER_STATUS_DISCONNECTED => Self::Disconnected,
            CloudFilters::CF_PROVIDER_STATUS_IDLE => Self::Idle,
            CloudFilters::CF_PROVIDER_STATUS_POPULATE_NAMESPACE => Self::PopulateNamespace,
            CloudFilters::CF_PROVIDER_STATUS_POPULATE_METADATA => Self::PopulateMetadata,
            CloudFilters::CF_PROVIDER_STATUS_POPULATE_CONTENT => Self::PopulateContent,
            CloudFilters::CF_PROVIDER_STATUS_SYNC_INCREMENTAL => Self::SyncIncremental,
            CloudFilters::CF_PROVIDER_STATUS_SYNC_FULL => Self::SyncFull,
            CloudFilters::CF_PROVIDER_STATUS_CONNECTIVITY_LOST => Self::ConnectivityLost,
            CloudFilters::CF_PROVIDER_STATUS_TERMINATED => Self::Terminated,
            CloudFilters::CF_PROVIDER_STATUS_ERROR => Self::Error,
            _ => Self::Unknown(status.0),
        }
    }
}

impl From<ProviderStatus> for CF_SYNC_PROVIDER_STATUS {
    fn from(status: ProviderStatus) -> Self {
        match status {
            ProviderStatus::Disconnected => CloudFilters::CF_PROVIDER_STATUS_DISCONNECTED,
            ProviderStatus::Idle => CloudFilters::CF_PROVIDER_STATUS_IDLE,
            ProviderStatus::PopulateNamespace => {
                CloudFilters::CF_PROVIDER_STATUS_POPULATE_NAMESPACE
            }
            ProviderStatus::PopulateMetadata => CloudFilters::CF_PROVIDER_STATUS_POPULATE_METADATA,
            ProviderStatus::PopulateContent => CloudFilters::CF_PROVIDER_STATUS_POPULATE_CONTENT,
            ProviderStatus::SyncIncremental => CloudFilters::CF_PROVIDER_STATUS_SYNC_INCREMENTAL,
            ProviderStatus::SyncFull => CloudFilters::CF_PROVIDER_STATUS_SYNC_FULL,
            ProviderStatus::ConnectivityLost => CloudFilters::CF_PROVIDER_STATUS_CONNECTIVITY_LOST,
            ProviderStatus::Terminated => CloudFilters::CF_PROVIDER_STATUS_TERMINATED,
            ProviderStatus::Error => CloudFilters::CF_PROVIDER_STATUS_ERROR,
            ProviderStatus::Unknown(value) => Self(value),
        }
    }
}
