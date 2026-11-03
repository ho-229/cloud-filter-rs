use std::{path::Path, ptr, slice};

use widestring::U16CString;
use windows::{
    core::{self, PCWSTR},
    Win32::{
        Foundation,
        Storage::CloudFilters::{self, CfCreatePlaceholders, CF_PLACEHOLDER_CREATE_INFO},
    },
};

use crate::{metadata::Metadata, sealed, usn::Usn};

/// A builder for creating new placeholder files/directories.
#[derive(Debug)]
#[repr(transparent)]
pub struct PlaceholderCreateInfo(CF_PLACEHOLDER_CREATE_INFO);

impl PlaceholderCreateInfo {
    /// Creates a new [PlaceholderCreateInfo].
    pub fn new(relative_path: impl AsRef<Path>) -> Self {
        Self(CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(
                U16CString::from_os_str(relative_path.as_ref())
                    .unwrap()
                    .into_raw(),
            ),
            Flags: CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_NONE,
            Result: Foundation::S_FALSE,
            ..Default::default()
        })
    }

    /// Marks the new directory as fully populated, disabling on-demand child enumeration.
    ///
    /// Use for an empty directory or one whose children will all be created locally.
    /// This does not prevent adding children later. Only applicable to placeholder directories.
    pub fn has_no_children(mut self) -> Self {
        self.0.Flags |= CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION;
        self
    }

    /// Marks a placeholder as in sync.
    ///
    /// See also
    /// [SetInSyncState](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfsetinsyncstate),
    /// [What does "In-Sync" Mean?](https://www.userfilesystem.com/programming/faq/#nav_whatdoesin-syncmean)
    pub fn mark_in_sync(mut self) -> Self {
        self.0.Flags |= CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
        self
    }

    /// Allows replacing an existing placeholder.
    pub fn overwrite(mut self) -> Self {
        self.0.Flags |= CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_SUPERSEDE;
        self
    }

    /// Blocks this placeholder file from being dehydrated.
    ///
    /// This flag does not work on directories.
    pub fn block_dehydration(mut self) -> Self {
        self.0.Flags |= CloudFilters::CF_PLACEHOLDER_CREATE_FLAG_ALWAYS_FULL;
        self
    }

    /// The metadata for the [PlaceholderCreateInfo].
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.0.FsMetadata = metadata.0;
        self
    }

    /// A buffer of bytes stored with the file that could be accessed through a
    /// [Request::file_blob][crate::filter::Request::file_blob] or
    /// [Placeholder::info][crate::placeholder::Placeholder::info].
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

        if !self.0.FileIdentity.is_null() {
            // Release the previous identity before replacing it. Drop owns the new one.
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    self.0.FileIdentity as *mut u8,
                    self.0.FileIdentityLength as usize,
                )));
            }
        }
        if blob.is_empty() {
            self.0.FileIdentity = ptr::null();
            self.0.FileIdentityLength = 0;
            return self;
        }

        let leaked_blob = Box::leak(blob.into_boxed_slice());
        self.0.FileIdentity = leaked_blob.as_ptr() as *const _;
        self.0.FileIdentityLength = leaked_blob.len() as _;

        self
    }

    pub fn result(&self) -> core::Result<Usn> {
        self.0.Result.ok().map(|_| self.0.CreateUsn as _)
    }

    /// Creates a placeholder file/directory on the file system.
    ///
    /// The value returned is the final [Usn] after the placeholder is created.
    ///
    /// It is recommended to use this function over
    /// [Placeholder][crate::placeholder::Placeholder::convert_to_placeholder] for efficiency purposes. If you
    /// need to create multiple placeholders, consider using [BatchCreate::create].
    ///
    /// If you need to create placeholders from the
    /// [SyncFilter::fetch_placeholders][crate::filter::SyncFilter::fetch_placeholders] callback,
    /// do not use this method. Instead, use
    /// [FetchPlaceholders::pass_with_placeholders][crate::filter::ticket::FetchPlaceholders::pass_with_placeholders].
    pub fn create<P: AsRef<Path>>(mut self, parent: P) -> core::Result<Usn> {
        unsafe {
            CfCreatePlaceholders(
                PCWSTR(U16CString::from_os_str(parent.as_ref()).unwrap().as_ptr()),
                slice::from_mut(&mut self.0),
                CloudFilters::CF_CREATE_FLAG_NONE,
                None,
            )?;
        }

        self.result()
    }
}

impl Drop for PlaceholderCreateInfo {
    fn drop(&mut self) {
        // Reclaim the NUL-terminated buffer transferred by U16CString::into_raw in new().
        drop(unsafe { U16CString::from_raw(self.0.RelativeFileName.0 as *mut u16) });

        if !self.0.FileIdentity.is_null() {
            // blob() transferred this Box<[u8]> allocation with exactly FileIdentityLength bytes.
            drop(unsafe {
                Box::from_raw(ptr::slice_from_raw_parts_mut(
                    self.0.FileIdentity as *mut u8,
                    self.0.FileIdentityLength as _,
                ))
            });
        }
    }
}

/// Creates multiple placeholder file/directories within the given path.
///
/// Attempts every entry and returns the first error, if any. Each entry's
/// [PlaceholderCreateInfo::result] retains its own result. Failures do not roll
/// back successfully created entries.
pub trait BatchCreate: sealed::Sealed {
    fn create<P: AsRef<Path>>(&mut self, path: P) -> core::Result<()>;
}

impl BatchCreate for [PlaceholderCreateInfo] {
    fn create<P: AsRef<Path>>(&mut self, path: P) -> core::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        let path = U16CString::from_os_str(path.as_ref()).unwrap();
        create_individually(self, |entry| unsafe {
            CfCreatePlaceholders(
                PCWSTR(path.as_ptr()),
                slice::from_mut(entry),
                CloudFilters::CF_CREATE_FLAG_NONE,
                None,
            )
        })
    }
}

impl sealed::Sealed for [PlaceholderCreateInfo] {}

pub(crate) fn create_individually(
    placeholders: &mut [PlaceholderCreateInfo],
    mut create: impl FnMut(&mut CF_PLACEHOLDER_CREATE_INFO) -> core::Result<()>,
) -> core::Result<()> {
    // CldFlt on Windows 10 build 19044 can retain compression flags between
    // batch entries, corrupting even equal-length identities. Isolate each
    // native call; sorting by identity length is insufficient.
    // https://learn.microsoft.com/en-us/answers/questions/6057324/
    let mut result = Ok(());
    for entry in placeholders {
        let created = create(&mut entry.0);
        // CfExecute can succeed while the individual entry failed.
        // Keep attempting later entries, preserving the first error.
        result = result.and(created).and(entry.0.Result.ok());
    }
    result
}
