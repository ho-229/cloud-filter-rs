use std::ops::{Range, RangeBounds};

use windows::{
    core,
    Win32::Storage::CloudFilters::{
        self, CfConvertToPlaceholder, CfHydratePlaceholder, CfSetInSyncState, CfSetPinState,
        CfUpdatePlaceholder, CF_CONVERT_FLAGS, CF_FILE_RANGE, CF_SET_PIN_FLAGS, CF_UPDATE_FLAGS,
    },
};

use super::{PinState, Placeholder};
use crate::{metadata::Metadata, usn::Usn, utility};

/// The placeholder pin flags.
#[derive(Debug, Clone, Copy)]
pub struct PinOptions(CF_SET_PIN_FLAGS);

impl PinOptions {
    /// Applies the pin state to all descendants of the placeholder (if the placeholder is a
    /// directory).
    pub fn recurse(&mut self) -> &mut Self {
        self.0 |= CloudFilters::CF_SET_PIN_FLAG_RECURSE;
        self
    }

    /// Applies the pin state to all descendants of the placeholder excluding the current one (if
    /// the placeholder is a directory).
    pub fn recurse_children(&mut self) -> &mut Self {
        self.0 |= CloudFilters::CF_SET_PIN_FLAG_RECURSE_ONLY;
        self
    }

    /// Stop applying the pin state when the first error is encountered. Otherwise, skip over it
    /// and keep applying.
    pub fn stop_on_error(&mut self) -> &mut Self {
        self.0 |= CloudFilters::CF_SET_PIN_FLAG_RECURSE_STOP_ON_ERROR;
        self
    }
}

impl Default for PinOptions {
    fn default() -> Self {
        Self(CloudFilters::CF_SET_PIN_FLAG_NONE)
    }
}

/// File to placeholder file conversion parameters.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    flags: CF_CONVERT_FLAGS,
    blob: Vec<u8>,
}

impl ConvertOptions {
    /// Marks a placeholder as in sync.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_in_sync(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_MARK_IN_SYNC;
        self
    }

    /// Dehydrate the placeholder after conversion.
    ///
    /// Files only. Conversion with dehydration requires exclusive access, for
    /// example through [`super::OpenOptions::exclusive`].
    pub fn dehydrate(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_DEHYDRATE;
        self
    }

    /// Marks the directory as partially populated, enabling
    /// [SyncFilter::fetch_placeholders][crate::filter::SyncFilter::fetch_placeholders]
    /// when a later access needs its children.
    ///
    /// Only applicable to placeholder directories.
    pub fn has_children(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_ENABLE_ON_DEMAND_POPULATION;
        self
    }

    /// Blocks this placeholder from being dehydrated.
    ///
    /// This flag does not work on directories.
    pub fn block_dehydration(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_ALWAYS_FULL;
        self
    }

    /// Forces the conversion of a non-cloud placeholder file to a cloud placeholder file.
    ///
    /// Placeholder files are built into the NTFS file system and thus, a placeholder not associated
    /// with the sync root is possible.
    pub fn force(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_FORCE_CONVERT_TO_CLOUD_FILE;
        self
    }

    /// A buffer of bytes stored with the file that could be accessed through a
    /// [Request::file_blob][crate::filter::Request::file_blob] or [Placeholder::info].
    ///
    /// The buffer must not exceed
    /// [4KiB](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/CloudFilters/constant.CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH.html).
    pub fn blob(mut self, blob: Vec<u8>) -> Self {
        assert!(
            blob.len() <= CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize,
            "blob size must not exceed {} bytes, got {} bytes",
            CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH,
            blob.len()
        );
        self.blob = blob;
        self
    }
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            flags: CloudFilters::CF_CONVERT_FLAG_NONE,
            blob: Vec::new(),
        }
    }
}

/// Placeholder update parameters.
#[derive(Debug, Clone)]
pub struct UpdateOptions<'a> {
    metadata: Option<Metadata>,
    dehydrate_ranges: Vec<CF_FILE_RANGE>,
    flags: CF_UPDATE_FLAGS,
    blob: &'a [u8],
}

impl<'a> UpdateOptions<'a> {
    /// Applies metadata while preserving timestamps and attributes whose supplied values are zero.
    ///
    /// File size has no preserve sentinel: a zero size truncates the file. Omitting
    /// this option entirely leaves the current metadata unchanged.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.flags &= !(CloudFilters::CF_UPDATE_FLAG_PASSTHROUGH_FS_METADATA);
        self.metadata = Some(metadata);
        self
    }

    /// Passes metadata to the filesystem without CFAPI's zero-field filtering.
    /// The underlying filesystem determines how each supplied value is interpreted.
    pub fn metadata_all(mut self, metadata: Metadata) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_PASSTHROUGH_FS_METADATA;
        self.metadata = Some(metadata);
        self
    }

    /// Byte ranges to invalidate and dehydrate during the update.
    ///
    /// Offsets and lengths must be page-aligned. The update requires exclusive
    /// file access. For the whole file, use [`Self::dehydrate`], which overrides
    /// these ranges; this method accepts finite Rust ranges, not the `CF_EOF` sentinel.
    ///
    /// # Panics
    /// Panics for reversed ranges or offsets outside the signed 64-bit Windows range.
    pub fn dehydrate_ranges(mut self, ranges: impl IntoIterator<Item = Range<u64>>) -> Self {
        self.dehydrate_ranges
            .extend(ranges.into_iter().map(|range| {
                let (start, length) =
                    utility::checked_range(range).expect("valid dehydration range");
                CF_FILE_RANGE {
                    StartingOffset: start,
                    Length: length,
                }
            }));
        self
    }

    /// The update will fail if the `IN_SYNC` attribute is not currently set on the placeholder.
    pub fn update_if_in_sync(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_VERIFY_IN_SYNC;
        self
    }

    /// Marks a placeholder as in sync.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_in_sync(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_MARK_IN_SYNC;
        self
    }

    /// Clears the placeholder's in-sync state without uploading or downloading data.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_not_in_sync(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_CLEAR_IN_SYNC;
        self
    }

    /// Dehydrates the entire file after a successful update, overriding any explicit ranges.
    /// Requires exclusive file access, for example through [`super::OpenOptions::exclusive`].
    pub fn dehydrate(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_DEHYDRATE;
        self
    }

    /// Marks a directory fully populated, disabling on-demand child enumeration.
    /// It need not be empty, and children may still be added later.
    pub fn has_no_children(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
        self
    }

    /// Enable on-demand population for directories.
    pub fn has_children(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_ENABLE_ON_DEMAND_POPULATION;
        self
    }

    /// Remove the file identity from the placeholder.
    /// [UpdateOptions::blob()](crate::placeholder::UpdateOptions::blob) will be ignored.
    pub fn remove_blob(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_REMOVE_FILE_IDENTITY;
        self
    }

    /// The platform removes all existing extrinsic properties on the placeholder.
    pub fn remove_properties(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_REMOVE_PROPERTY;
        self
    }

    /// Replaces the identity blob. An empty slice preserves it; use [`Self::remove_blob`]
    /// to clear it. Panics if the supplied blob exceeds 4 KiB.
    pub fn blob(mut self, blob: &'a [u8]) -> Self {
        assert!(
            blob.len() <= CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize,
            "blob size must not exceed {} bytes, got {} bytes",
            CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH,
            blob.len()
        );
        self.blob = blob;
        self
    }
}

impl Default for UpdateOptions<'_> {
    fn default() -> Self {
        Self {
            metadata: None,
            dehydrate_ranges: Vec::new(),
            flags: CloudFilters::CF_UPDATE_FLAG_NONE,
            blob: &[],
        }
    }
}

impl Placeholder {
    /// Marks a placeholder as in sync or not.
    ///
    /// A supplied nonzero [Usn] must match the file's current USN. `None` or zero
    /// skips that check. On success, a supplied USN receives the resulting value.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_in_sync<'a>(
        &mut self,
        in_sync: bool,
        usn: impl Into<Option<&'a mut Usn>>,
    ) -> core::Result<&mut Self> {
        unsafe {
            CfSetInSyncState(
                self.handle.handle,
                match in_sync {
                    true => CloudFilters::CF_IN_SYNC_STATE_IN_SYNC,
                    false => CloudFilters::CF_IN_SYNC_STATE_NOT_IN_SYNC,
                },
                CloudFilters::CF_SET_IN_SYNC_FLAG_NONE,
                usn.into().map(|x| x as *mut _),
            )
        }?;

        Ok(self)
    }

    /// Sets the pin state of the placeholder.
    /// Success confirms the state change, not completion of hydration or dehydration.
    ///
    /// See also
    /// [CfSetPinState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetpinstate),
    /// [What does "Pinned" Mean?](https://www.userfilesystem.com/programming/faq/#nav_howdoesthealwayskeeponthisdevicemenuworks)
    pub fn mark_pin(&mut self, state: PinState, options: PinOptions) -> core::Result<&mut Self> {
        unsafe { CfSetPinState(self.handle.handle, state.into(), options.0, None) }?;
        Ok(self)
    }

    /// Converts an existing file or directory to a placeholder, preserving its metadata.
    ///
    /// A supplied [Usn] receives the value after conversion; its input value is ignored.
    /// If the options request dehydration, the caller must hold exclusive file access.
    ///
    /// See also [CfConvertToPlaceholder](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfconverttoplaceholder).
    pub fn convert_to_placeholder<'a>(
        &mut self,
        options: ConvertOptions,
        usn: impl Into<Option<&'a mut Usn>>,
    ) -> core::Result<&mut Self> {
        unsafe {
            CfConvertToPlaceholder(
                self.handle.handle,
                (!options.blob.is_empty()).then_some(options.blob.as_ptr() as *const _),
                options.blob.len() as _,
                options.flags,
                usn.into().map(|x| x as *mut _),
                None,
            )
        }?;

        Ok(self)
    }

    /// Updates various characteristics of a placeholder.
    ///
    /// A supplied nonzero [Usn] guards against intervening changes and receives the
    /// resulting USN on success. `None` or zero skips the comparison.
    /// Updates that dehydrate data require exclusive file access.
    ///
    /// See also [CfUpdatePlaceholder](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfupdateplaceholder).
    pub fn update<'a>(
        &mut self,
        options: UpdateOptions,
        usn: impl Into<Option<&'a mut Usn>>,
    ) -> core::Result<&mut Self> {
        unsafe {
            CfUpdatePlaceholder(
                self.handle.handle,
                options.metadata.as_ref().map(|x| &x.0 as *const _),
                (!options.blob.is_empty()).then_some(options.blob.as_ptr() as *const _),
                options.blob.len() as _,
                (!options.dehydrate_ranges.is_empty()).then_some(&options.dehydrate_ranges),
                options.flags,
                usn.into().map(|u| u as *mut _),
                None,
            )
        }?;

        Ok(self)
    }

    /// Hydrates a placeholder file by ensuring that the specified byte range is present on-disk
    /// in the placeholder. This is valid for files only.
    ///
    /// Returns E_INVALIDARG for reversed, overflowing, or unrepresentable ranges.
    ///
    /// See also [CfHydratePlaceholder](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfhydrateplaceholder)
    /// and [discussion](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfhydrateplaceholder#remarks).
    pub fn hydrate(&mut self, range: impl RangeBounds<u64>) -> core::Result<()> {
        let (start, length) = utility::checked_range(range)?;
        unsafe {
            CfHydratePlaceholder(
                self.handle.handle,
                start,
                length,
                CloudFilters::CF_HYDRATE_FLAG_NONE,
                None,
            )
        }
    }
}
