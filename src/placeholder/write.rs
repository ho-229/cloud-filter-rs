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
    /// This flag is only applicable to files.
    pub fn dehydrate(mut self) -> Self {
        self.flags |= CloudFilters::CF_CONVERT_FLAG_DEHYDRATE;
        self
    }

    /// Marks the placeholder as "partially full," such that
    /// [SyncFilter::fetch_placeholders][crate::filter::SyncFilter::fetch_placeholders]
    /// will be invoked when this directory is next accessed so that the remaining placeholders are inserted.
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
    /// [Metadata] contains file system metadata about the placeholder to be updated.
    ///
    /// File size will be truncates to 0 if not specified, otherwise to the specified size.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.flags &= !(CloudFilters::CF_UPDATE_FLAG_PASSTHROUGH_FS_METADATA);
        self.metadata = Some(metadata);
        self
    }

    /// Fields in [Metadata] will be updated.
    pub fn metadata_all(mut self, metadata: Metadata) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_PASSTHROUGH_FS_METADATA;
        self.metadata = Some(metadata);
        self
    }

    /// Extended ranges to be dehydrated.
    ///
    /// All the offsets and lengths should be `PAGE_SIZE` aligned.
    /// Passing a single range with Offset `0` and Length `CF_EOF` will invalidate the entire file.
    /// This has the same effect as passing the flag `CF_UPDATE_FLAG_DEHYDRATE` instead.
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

    /// Marks a placeholder as not in sync. `Sync Pending` will be shown in explorer.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_not_in_sync(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_CLEAR_IN_SYNC;
        self
    }

    /// The platform dehydrates the file after updating the placeholder successfully.
    pub fn dehydrate(mut self) -> Self {
        self.flags |= CloudFilters::CF_UPDATE_FLAG_DEHYDRATE;
        self
    }

    /// Disables on-demand population for directories.
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
    /// If the passed [Usn] is outdated, the call will fail,
    /// otherwise the [Usn] will be updated.
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
    ///
    /// See also
    /// [CfSetPinState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetpinstate),
    /// [What does "Pinned" Mean?](https://www.userfilesystem.com/programming/faq/#nav_howdoesthealwayskeeponthisdevicemenuworks)
    pub fn mark_pin(&mut self, state: PinState, options: PinOptions) -> core::Result<&mut Self> {
        unsafe { CfSetPinState(self.handle.handle, state.into(), options.0, None) }?;
        Ok(self)
    }

    /// Converts a file to a placeholder file.
    ///
    /// If the passed [Usn] is outdated, the call will fail,
    /// otherwise the [Usn] will be updated.
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
