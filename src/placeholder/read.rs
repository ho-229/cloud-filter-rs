use std::{
    mem,
    ops::{Range, RangeBounds},
};

use windows::{
    core,
    Wdk::Storage::FileSystem::RtlSetThreadPlaceholderCompatibilityMode,
    Win32::{
        Foundation::{
            ERROR_HANDLE_EOF, ERROR_INVALID_DATA, ERROR_MORE_DATA, ERROR_NOT_A_CLOUD_FILE,
        },
        Storage::{
            CloudFilters::{
                self, CfGetPlaceholderInfo, CfGetPlaceholderRangeInfo,
                CfGetPlaceholderStateFromFileInfo, CF_FILE_RANGE, CF_PLACEHOLDER_RANGE_INFO_CLASS,
                CF_PLACEHOLDER_STANDARD_INFO, CF_PLACEHOLDER_STATE,
            },
            FileSystem::{
                FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_TAG_INFO,
            },
        },
    },
};

use super::{PinState, Placeholder};
use crate::utility;

pub struct PlaceholderInfo {
    data: Vec<u8>,
    info: CF_PLACEHOLDER_STANDARD_INFO,
}

impl Clone for PlaceholderInfo {
    fn clone(&self) -> Self {
        Self::new(self.data.clone())
    }
}

impl PlaceholderInfo {
    fn new(data: Vec<u8>) -> Self {
        let header = &data[..mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>()];
        // The buffer contains an initialized header, but Vec<u8> does not guarantee its
        // alignment. Own a copy so cloning never retains a pointer into another buffer.
        let info = unsafe {
            header
                .as_ptr()
                .cast::<CF_PLACEHOLDER_STANDARD_INFO>()
                .read_unaligned()
        };
        Self { data, info }
    }

    pub fn on_disk_data_size(&self) -> i64 {
        self.info.OnDiskDataSize
    }

    pub fn validated_data_size(&self) -> i64 {
        self.info.ValidatedDataSize
    }

    pub fn modified_data_size(&self) -> i64 {
        self.info.ModifiedDataSize
    }

    pub fn properties_size(&self) -> i64 {
        self.info.PropertiesSize
    }

    pub fn pin_state(&self) -> PinState {
        self.info.PinState.into()
    }

    pub fn is_in_sync(&self) -> bool {
        self.info.InSyncState == CloudFilters::CF_IN_SYNC_STATE_IN_SYNC
    }

    pub fn file_id(&self) -> i64 {
        self.info.FileId
    }

    pub fn sync_root_file_id(&self) -> i64 {
        self.info.SyncRootFileId
    }

    pub fn blob(&self) -> &[u8] {
        // The identity starts at its field, inside the structure's size (it is declared as a
        // one-byte array), and is as long as the platform says.
        let start = mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
        let len = self.info.FileIdentityLength as usize;
        self.data.get(start..start + len).unwrap_or_default()
    }
}

impl std::fmt::Debug for PlaceholderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceholderInfo")
            .field("on_disk_data_size", &self.on_disk_data_size())
            .field("validated_data_size", &self.validated_data_size())
            .field("modified_data_size", &self.modified_data_size())
            .field("properties_size", &self.properties_size())
            .field("pin_state", &self.pin_state())
            .field("is_in_sync", &self.is_in_sync())
            .field("file_id", &self.file_id())
            .field("sync_root_file_id", &self.sync_root_file_id())
            .finish()
    }
}

/// The type of data to read from a placeholder.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ReadType {
    /// Any data that is saved to the disk.
    Any,
    /// Data that has been synced to the cloud.
    Validated,
    /// Data that has not synced to the cloud.
    Modified,
}

impl From<ReadType> for CF_PLACEHOLDER_RANGE_INFO_CLASS {
    fn from(read_type: ReadType) -> Self {
        match read_type {
            ReadType::Any => CloudFilters::CF_PLACEHOLDER_RANGE_INFO_ONDISK,
            ReadType::Validated => CloudFilters::CF_PLACEHOLDER_RANGE_INFO_VALIDATED,
            ReadType::Modified => CloudFilters::CF_PLACEHOLDER_RANGE_INFO_MODIFIED,
        }
    }
}

/// A snapshot of the placeholder flags reported by Windows file attributes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderState(CF_PLACEHOLDER_STATE);

impl PlaceholderState {
    /// The directory is a sync root.
    pub fn sync_root(&self) -> bool {
        (self.0 & CloudFilters::CF_PLACEHOLDER_STATE_SYNC_ROOT).0 != 0
    }

    /// The placeholder has an essential property in its property store.
    pub fn essential_prop_present(&self) -> bool {
        (self.0 & CloudFilters::CF_PLACEHOLDER_STATE_ESSENTIAL_PROP_PRESENT).0 != 0
    }

    /// Windows reports the placeholder as in sync with the cloud.
    pub fn in_sync(&self) -> bool {
        (self.0 & CloudFilters::CF_PLACEHOLDER_STATE_IN_SYNC).0 != 0
    }

    /// Content is not yet ready for use, possibly because validation is pending.
    pub fn partial(&self) -> bool {
        (self.0 & CloudFilters::CF_PLACEHOLDER_STATE_PARTIAL).0 != 0
    }

    /// Some or all file content is absent from local storage.
    pub fn partial_on_disk(&self) -> bool {
        (self.0 & CloudFilters::CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK).0 != 0
    }
}

impl std::fmt::Debug for PlaceholderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaceholderState")
            .field("sync_root", &self.sync_root())
            .field("essential_prop_present", &self.essential_prop_present())
            .field("in_sync", &self.in_sync())
            .field("partial", &self.partial())
            .field("partial_on_disk", &self.partial_on_disk())
            .finish()
    }
}

impl Placeholder {
    /// Gets various characteristics of the placeholder.
    ///
    /// Returns [None] if the handle not points to a placeholder.
    ///
    /// If the placeholder blob size is known, use [fixed_size_info](Self::fixed_size_info) instead.
    pub fn info(&self) -> core::Result<Option<PlaceholderInfo>> {
        let mut info_size = 0;
        let mut data = vec![0u8; mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + 4096];
        let r = unsafe {
            CfGetPlaceholderInfo(
                self.handle.handle,
                CloudFilters::CF_PLACEHOLDER_INFO_STANDARD,
                data.as_mut_ptr() as *mut _,
                data.len() as _,
                Some(&mut info_size),
            )
        };

        match r {
            Ok(()) => {
                // The platform may report less than the structure's size, as it does for a
                // folder without an identity blob; the rest of the buffer is zeroes.
                let len = (info_size as usize).max(mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>());
                data.truncate(len);
                data.shrink_to_fit();

                Ok(Some(PlaceholderInfo::new(data)))
            }
            Err(e) if e.code() == ERROR_NOT_A_CLOUD_FILE.to_hresult() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Gets various characteristics of the placeholder.
    ///
    /// If the `blob_size` not matches the actual size of the blob,
    /// the call will returns `HRESULT_FROM_WIN32(ERROR_MORE_DATA)`.
    /// Returns [None] if the handle not points to a placeholder.
    pub fn fixed_size_info(&self, blob_size: usize) -> core::Result<Option<PlaceholderInfo>> {
        if blob_size > CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        }
        let mut data = vec![0; mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + blob_size];

        let r = unsafe {
            CfGetPlaceholderInfo(
                self.handle.handle,
                CloudFilters::CF_PLACEHOLDER_INFO_STANDARD,
                data.as_mut_ptr() as *mut _,
                data.len() as u32,
                None,
            )
        };

        match r {
            Ok(()) => Ok(Some(PlaceholderInfo::new(data))),
            Err(e) if e.code() == ERROR_NOT_A_CLOUD_FILE.to_hresult() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Returns the on-disk, validated, or modified byte ranges within `range`.
    ///
    /// Queries through EOF for an unbounded end, and handles paginated native results.
    /// Empty ranges and queries starting at or beyond EOF return an empty vector.
    /// This only reads range metadata; it does not hydrate the file. Returned ranges
    /// may have coarser granularity than individual writes. Callers must prevent
    /// concurrent changes if they need a consistent snapshot across query pages.
    ///
    /// See also [CfGetPlaceholderRangeInfo].
    pub fn ranges(
        &self,
        read_type: ReadType,
        range: impl RangeBounds<u64>,
    ) -> core::Result<Vec<Range<u64>>> {
        let (mut offset, length) = utility::checked_range(range)?;
        let end = if length == -1 {
            i64::MAX
        } else {
            offset + length
        };
        let mut ranges = Vec::new();
        while offset < end {
            let mut buffer = [CF_FILE_RANGE::default(); 64];
            let mut returned = 0;
            let result = unsafe {
                CfGetPlaceholderRangeInfo(
                    self.handle.handle,
                    read_type.into(),
                    offset,
                    if length == -1 { -1 } else { end - offset },
                    buffer.as_mut_ptr().cast(),
                    mem::size_of_val(&buffer) as u32,
                    Some(&mut returned),
                )
            };
            let more = match result {
                Ok(()) => false,
                Err(error) if error.code() == ERROR_HANDLE_EOF.to_hresult() => break,
                Err(error) if error.code() == ERROR_MORE_DATA.to_hresult() => true,
                Err(error) => return Err(error),
            };
            let invalid = || core::Error::from(ERROR_INVALID_DATA);
            if returned as usize > mem::size_of_val(&buffer)
                || !(returned as usize).is_multiple_of(mem::size_of::<CF_FILE_RANGE>())
            {
                return Err(invalid());
            }
            let previous = offset;
            for entry in &buffer[..returned as usize / mem::size_of::<CF_FILE_RANGE>()] {
                let entry_end = entry
                    .StartingOffset
                    .checked_add(entry.Length)
                    .ok_or_else(invalid)?;
                if entry.StartingOffset < 0 || entry.Length <= 0 || entry_end <= offset {
                    return Err(invalid());
                }
                let start = entry.StartingOffset.max(offset);
                let stop = entry_end.min(end);
                if start < stop {
                    ranges.push(start as u64..stop as u64);
                }
                offset = entry_end;
            }
            if !more {
                break;
            }
            // Never loop forever if a partial response contains no usable entries.
            if offset <= previous {
                return Err(invalid());
            }
        }
        Ok(ranges)
    }

    /// Reads placeholder flags without hydrating the file.
    ///
    /// Returns `None` for an ordinary file. Supports both protected CF handles and
    /// ordinary Win32 handles. For detailed sync state and byte counts, use [`Self::info`].
    /// Temporarily exposes placeholder attributes on the calling thread, restoring
    /// its previous compatibility mode before returning.
    pub fn state(&self) -> core::Result<Option<PlaceholderState>> {
        // Compatibility mode can hide cloud reparse tags even from a valid
        // placeholder handle. Expose them only on this thread, for this query.
        const PHCM_EXPOSE_PLACEHOLDERS: i8 = 2;
        struct RestoreMode(i8);
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                unsafe { RtlSetThreadPlaceholderCompatibilityMode(self.0) };
            }
        }
        let previous =
            unsafe { RtlSetThreadPlaceholderCompatibilityMode(PHCM_EXPOSE_PLACEHOLDERS) };
        if previous < 0 {
            return Err(core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "could not expose placeholder attributes on this thread",
            ));
        }
        let _restore = RestoreMode(previous);
        let handle = self.win32_handle()?;
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        let state = unsafe {
            GetFileInformationByHandleEx(
                handle.handle(),
                FileAttributeTagInfo,
                (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                mem::size_of_val(&info) as u32,
            )?;
            CfGetPlaceholderStateFromFileInfo(
                (&info as *const FILE_ATTRIBUTE_TAG_INFO).cast(),
                FileAttributeTagInfo,
            )
        };
        match state {
            CloudFilters::CF_PLACEHOLDER_STATE_INVALID => Err(core::Error::from_win32()),
            CloudFilters::CF_PLACEHOLDER_STATE_NO_STATES => Ok(None),
            state => Ok(Some(PlaceholderState(state))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder_info(blob: &[u8]) -> PlaceholderInfo {
        let mut data = vec![0; mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + blob.len()];
        let file_id_offset = mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileId);
        data[file_id_offset..file_id_offset + mem::size_of::<i64>()]
            .copy_from_slice(&42i64.to_ne_bytes());
        let length_offset = mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentityLength);
        data[length_offset..length_offset + mem::size_of::<u32>()]
            .copy_from_slice(&(blob.len() as u32).to_ne_bytes());
        let blob_offset = mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
        data[blob_offset..blob_offset + blob.len()].copy_from_slice(blob);
        PlaceholderInfo::new(data)
    }

    #[test]
    fn placeholder_info_clone_owns_metadata_and_identity() {
        let blob = b"file\0identity";
        let cloned = {
            let mut original = placeholder_info(blob);
            let cloned = original.clone();

            // Detect references to the original buffer before freeing it, so a broken
            // clone fails deterministically without reading freed memory.
            original.data.fill(0);
            assert_eq!(cloned.file_id(), 42);
            assert_eq!(cloned.blob(), blob);
            cloned
        };

        assert_eq!(cloned.file_id(), 42);
        assert_eq!(cloned.blob(), blob);
    }

    #[test]
    fn placeholder_info_preserves_identity_length() {
        for blob in [vec![], vec![0xab], vec![0xcd; 4096]] {
            let info = placeholder_info(&blob);
            assert_eq!(info.file_id(), 42);
            assert_eq!(info.blob(), blob);
        }
    }
}
