use std::{
    fs::File,
    mem::ManuallyDrop,
    os::windows::io::{FromRawHandle, IntoRawHandle},
    path::Path,
};

use widestring::U16CString;
use windows::{
    core::{self, PCWSTR},
    Win32::{
        Foundation::{E_HANDLE, HANDLE},
        Storage::CloudFilters::{self, CfOpenFileWithOplock, CF_OPEN_FILE_FLAGS},
    },
};

use super::{OwnedPlaceholderHandle, Placeholder, PlaceholderHandleType};

/// Options for opening a placeholder file/directory.
pub struct OpenOptions {
    flags: CF_OPEN_FILE_FLAGS,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn exclusive(mut self) -> Self {
        self.flags |= CloudFilters::CF_OPEN_FILE_FLAG_EXCLUSIVE;
        self
    }

    pub fn write_access(mut self) -> Self {
        self.flags |= CloudFilters::CF_OPEN_FILE_FLAG_WRITE_ACCESS;
        self
    }

    pub fn delete_access(mut self) -> Self {
        self.flags |= CloudFilters::CF_OPEN_FILE_FLAG_DELETE_ACCESS;
        self
    }

    pub fn foreground(mut self) -> Self {
        self.flags |= CloudFilters::CF_OPEN_FILE_FLAG_FOREGROUND;
        self
    }

    /// Open the placeholder file/directory using `CfOpenFileWithOplock`.
    pub fn open(self, path: impl AsRef<Path>) -> core::Result<Placeholder> {
        let u16_path = U16CString::from_os_str(path.as_ref()).unwrap();
        let handle = unsafe { CfOpenFileWithOplock(PCWSTR(u16_path.as_ptr()), self.flags) }?;
        Ok(Placeholder {
            handle: unsafe { OwnedPlaceholderHandle::from_cfapi(handle) },
        })
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            flags: CloudFilters::CF_OPEN_FILE_FLAG_NONE,
        }
    }
}

impl Placeholder {
    /// Create a placeholder from a raw handle.
    ///
    /// # Safety
    ///
    /// The passed handle must be a valid protected handle or win32 handle.
    pub unsafe fn from_raw_handle(handle: OwnedPlaceholderHandle) -> Self {
        Self { handle }
    }

    /// Open options for opening [Placeholder]s.
    pub fn options() -> OpenOptions {
        OpenOptions::default()
    }

    /// Open the placeholder file/directory with `CF_OPEN_FILE_FLAG_NONE`.
    pub fn open(path: impl AsRef<Path>) -> core::Result<Self> {
        OpenOptions::new().open(path)
    }
}

impl From<File> for Placeholder {
    fn from(file: File) -> Self {
        Self {
            handle: unsafe { OwnedPlaceholderHandle::from_win32(HANDLE(file.into_raw_handle())) },
        }
    }
}

impl TryFrom<Placeholder> for File {
    type Error = core::Error;

    /// Transfers an ordinary Win32 handle back to File.
    ///
    /// Protected CfAPI handles are asynchronous opaque handles, not owned synchronous
    /// File handles. They return E_HANDLE; use Placeholder::win32_handle for Win32 I/O.
    fn try_from(placeholder: Placeholder) -> core::Result<Self> {
        if placeholder.handle.handle_type != PlaceholderHandleType::Win32 {
            return Err(E_HANDLE.into());
        }
        let owner = placeholder.handle;
        let owner = ManuallyDrop::new(owner);
        // Ownership has moved to File; OwnedPlaceholderHandle must not close it.
        Ok(unsafe { File::from_raw_handle(owner.handle.0) })
    }
}
