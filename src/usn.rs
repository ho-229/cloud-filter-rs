use std::{
    mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
};

use windows::{
    core,
    Win32::{
        Foundation::{ERROR_INVALID_DATA, ERROR_IO_PENDING, HANDLE},
        System::{
            Ioctl::{FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA, USN_RECORD_V2},
            Threading::CreateEventW,
            IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED},
        },
    },
};

/// An update sequence number identifies a file's latest change-journal record.
/// It is not a per-write counter or a remote version identifier.
///
/// Passing a nonzero USN to [Placeholder::update][crate::placeholder::Placeholder::update]
/// guards against changes since that value was read. Zero or `None` skips the check.
pub type Usn = i64;

pub(crate) fn read(handle: RawHandle) -> core::Result<Usn> {
    let event = unsafe { CreateEventW(None, true, false, None)? };
    let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
    let mut overlapped = OVERLAPPED {
        // Suppress IOCP packets referencing this stack-local OVERLAPPED.
        hEvent: HANDLE((event.as_raw_handle() as usize | 1) as RawHandle),
        ..Default::default()
    };
    let versions = READ_FILE_USN_DATA {
        MinMajorVersion: 2,
        MaxMajorVersion: 2,
    };
    // A V2 record and the longest NTFS filename fit in this aligned buffer.
    let mut buffer = [0u64; 128];
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            HANDLE(handle),
            FSCTL_READ_FILE_USN_DATA,
            Some((&versions as *const READ_FILE_USN_DATA).cast()),
            mem::size_of_val(&versions) as u32,
            Some(buffer.as_mut_ptr().cast()),
            mem::size_of_val(&buffer) as u32,
            Some(&mut returned),
            Some(&mut overlapped),
        )
    };
    match result {
        // Also support FILE_FLAG_OVERLAPPED handles. Drain completion before
        // either buffer or OVERLAPPED leaves scope; synchronous handles ignore it.
        Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => unsafe {
            GetOverlappedResult(HANDLE(handle), &overlapped, &mut returned, true)?;
        },
        result => result?,
    }
    if (returned as usize) < mem::size_of::<USN_RECORD_V2>() {
        return Err(ERROR_INVALID_DATA.into());
    }
    // Copy the initialized header without borrowing its variable-length filename.
    let record = unsafe { buffer.as_ptr().cast::<USN_RECORD_V2>().read_unaligned() };
    if record.MajorVersion != 2
        || (record.RecordLength as usize) < mem::size_of::<USN_RECORD_V2>()
        || record.RecordLength > returned
    {
        return Err(ERROR_INVALID_DATA.into());
    }
    Ok(record.Usn)
}
