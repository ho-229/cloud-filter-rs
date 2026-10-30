use std::os::windows::io::{AsRawHandle, RawHandle};

use windows::{
    core,
    Win32::{
        Foundation::{CloseHandle, E_HANDLE, HANDLE},
        Storage::CloudFilters::{
            self, CfCloseHandle, CfGetWin32HandleFromProtectedHandle, CfReferenceProtectedHandle,
            CfReleaseProtectedHandle, CF_PIN_STATE,
        },
    },
};

mod open;
mod read;
mod write;

pub use open::OpenOptions;
pub use read::{PlaceholderInfo, PlaceholderState, ReadType};
pub use write::{ConvertOptions, PinOptions, UpdateOptions};

/// A struct to perform various operations on a placeholder(or regular) file/directory.
#[derive(Debug)]
pub struct Placeholder {
    handle: OwnedPlaceholderHandle,
}

impl Placeholder {
    /// Borrows a Win32 handle for ordinary Win32 APIs.
    ///
    /// For a protected CF handle, the returned guard holds a native reference until
    /// dropped. Keep it short-lived so oplock breaks can be acknowledged promptly.
    pub fn win32_handle(&self) -> core::Result<Win32Handle<'_>> {
        Win32Handle::new(&self.handle)
    }

    /// Returns the owned placeholder handle.
    pub fn inner_handle(&self) -> &OwnedPlaceholderHandle {
        &self.handle
    }
}

/// The pin state of a placeholder.
///
/// [Read more
/// here](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_pin_state#remarks)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// The platform could decide freely.
    Unspecified,
    /// [SyncFilter::fetch_data][crate::filter::SyncFilter::fetch_data] will be called to hydrate the rest
    /// of the placeholder's data. Any dehydration requests will fail automatically.
    Pinned,
    /// [SyncFilter::dehydrate][crate::filter::SyncFilter::dehydrate] will be called to dehydrate the rest
    /// of the placeholder's data.
    Unpinned,
    /// The placeholder will never sync to the cloud.
    Excluded,
    /// The placeholder will inherit the parent placeholder's pin state.
    Inherit,
}

impl From<PinState> for CF_PIN_STATE {
    fn from(state: PinState) -> Self {
        match state {
            PinState::Unspecified => CloudFilters::CF_PIN_STATE_UNSPECIFIED,
            PinState::Pinned => CloudFilters::CF_PIN_STATE_PINNED,
            PinState::Unpinned => CloudFilters::CF_PIN_STATE_UNPINNED,
            PinState::Excluded => CloudFilters::CF_PIN_STATE_EXCLUDED,
            PinState::Inherit => CloudFilters::CF_PIN_STATE_INHERIT,
        }
    }
}

impl From<CF_PIN_STATE> for PinState {
    fn from(state: CF_PIN_STATE) -> Self {
        match state {
            CloudFilters::CF_PIN_STATE_UNSPECIFIED => PinState::Unspecified,
            CloudFilters::CF_PIN_STATE_PINNED => PinState::Pinned,
            CloudFilters::CF_PIN_STATE_UNPINNED => PinState::Unpinned,
            CloudFilters::CF_PIN_STATE_EXCLUDED => PinState::Excluded,
            CloudFilters::CF_PIN_STATE_INHERIT => PinState::Inherit,
            _ => unreachable!(),
        }
    }
}

/// The API responsible for closing an owned placeholder handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderHandleType {
    /// A protected handle returned by [`CfOpenFileWithOplock`](windows::Win32::Storage::CloudFilters::CfOpenFileWithOplock).
    CfApi,
    /// An ordinary Win32 file handle.
    Win32,
}

/// An owned file handle, closed with the API that created it.
#[derive(Debug)]
pub struct OwnedPlaceholderHandle {
    handle_type: PlaceholderHandleType,
    handle: HANDLE,
}

// Windows file and protected handles may be used and closed on any thread.
// Ownership controls the final close; operations use the OS synchronization.
unsafe impl Send for OwnedPlaceholderHandle {}
unsafe impl Sync for OwnedPlaceholderHandle {}

impl OwnedPlaceholderHandle {
    /// Takes ownership of a handle returned by [`CfOpenFileWithOplock`](windows::Win32::Storage::CloudFilters::CfOpenFileWithOplock).
    ///
    /// # Safety
    /// The handle must be valid and exclusively owned by the caller.
    pub unsafe fn from_cfapi(handle: HANDLE) -> Self {
        Self {
            handle_type: PlaceholderHandleType::CfApi,
            handle,
        }
    }

    /// Takes ownership of an ordinary Win32 file handle.
    ///
    /// # Safety
    /// The handle must be valid and exclusively owned by the caller, and must be
    /// synchronous (opened without `FILE_FLAG_OVERLAPPED`) for use with `std::fs::File`.
    pub unsafe fn from_win32(handle: HANDLE) -> Self {
        Self {
            handle_type: PlaceholderHandleType::Win32,
            handle,
        }
    }

    pub const fn handle(&self) -> HANDLE {
        self.handle
    }

    pub const fn handle_type(&self) -> PlaceholderHandleType {
        self.handle_type
    }
}

impl Drop for OwnedPlaceholderHandle {
    fn drop(&mut self) {
        match self.handle_type {
            PlaceholderHandleType::CfApi => unsafe { CfCloseHandle(self.handle) },
            PlaceholderHandleType::Win32 => unsafe {
                _ = CloseHandle(self.handle);
            },
        }
    }
}

/// A borrowed Win32 handle valid for the lifetime of this guard.
///
/// For protected CF handles, this pairs `CfReferenceProtectedHandle` with
/// `CfReleaseProtectedHandle`. Keep the guard short-lived: holding it delays
/// acknowledgement of oplock breaks. Ordinary handles are simply borrowed.
/// The guard cannot outlive its [`Placeholder`].
///
/// ```compile_fail
/// use cloud_filter::placeholder::Placeholder;
/// let placeholder = Placeholder::open("file.txt").unwrap();
/// let handle = placeholder.win32_handle().unwrap();
/// drop(placeholder); // Still borrowed by handle.
/// let _ = handle.handle();
/// ```
#[derive(Debug)]
pub struct Win32Handle<'a> {
    owner: &'a OwnedPlaceholderHandle,
    handle: HANDLE,
}

impl<'a> Win32Handle<'a> {
    fn new(owner: &'a OwnedPlaceholderHandle) -> core::Result<Self> {
        if owner.handle_type == PlaceholderHandleType::Win32 {
            return Ok(Self {
                owner,
                handle: owner.handle,
            });
        }
        if !unsafe { CfReferenceProtectedHandle(owner.handle) }.as_bool() {
            return Err(E_HANDLE.into());
        }
        let guard = Self {
            owner,
            handle: unsafe { CfGetWin32HandleFromProtectedHandle(owner.handle) },
        };
        if guard.handle.is_invalid() {
            // Drop also balances the native reference on this error path.
            return Err(E_HANDLE.into());
        }
        Ok(guard)
    }

    /// The borrowed Win32 handle. Do not close it or use it after dropping the guard.
    pub fn handle(&self) -> HANDLE {
        self.handle
    }
}

impl AsRawHandle for Win32Handle<'_> {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.0
    }
}

impl Drop for Win32Handle<'_> {
    fn drop(&mut self) {
        if self.owner.handle_type == PlaceholderHandleType::CfApi {
            unsafe { CfReleaseProtectedHandle(self.owner.handle) };
        }
    }
}

// Referenced Windows handles may be used and released on another thread.
// The borrow prevents the owner from being closed before this guard is dropped.
unsafe impl Send for Win32Handle<'_> {}
unsafe impl Sync for Win32Handle<'_> {}
