use std::{
    future::Future,
    ops::{Bound, RangeBounds},
    pin::Pin,
};

use windows::core::{self, HSTRING};

use crate::sealed;

pub use nt_time::FileTime;

// TODO: add something to convert an Option<T> to a *const T and *mut T

pub(crate) trait ToHString
where
    Self: AsRef<[u16]>,
{
    /// Converts a 16-bit buffer to a Windows reference-counted [HSTRING][windows::core::HSTRING].
    ///
    /// # Panics
    ///
    /// Panics if [HeapAlloc](https://docs.microsoft.com/en-us/windows/win32/api/heapapi/nf-heapapi-heapalloc) fails.
    fn to_hstring(&self) -> HSTRING {
        HSTRING::from_wide(self.as_ref()).unwrap()
    }
}

impl<T: AsRef<[u16]>> ToHString for T {}

/// A trait for types that can read data from a file-like object at a specific offset.
///
/// This is a low-level interface that is used by Cloud Filter to implement the
/// [CfExecute](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfexecute)
/// function's `CF_OPERATION_TYPE_RETRIEVE_DATA` operation.
///
/// You should not need to implement this trait yourself, but rather use the
/// [utility::ReadAt](crate::utility::ReadAt) trait as a bound for your argument.
pub trait ReadAt: sealed::Sealed {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> core::Result<u64>;
}

/// A trait for types that can write data to a file-like object at a specific offset.
///
/// This is a low-level interface that is used by Cloud Filter to implement the
/// [CfExecute](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfexecute)
/// function's `CF_OPERATION_TYPE_TRANSFER_DATA` operation.
///
/// You should not need to implement this trait yourself, but rather use the
/// [utility::WriteAt](crate::utility::WriteAt) trait as a bound for your argument.
pub trait WriteAt: sealed::Sealed {
    fn write_at(&self, buf: &[u8], offset: u64) -> core::Result<()>;
}

pub(crate) type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(crate) fn checked_offset(value: u64) -> core::Result<i64> {
    i64::try_from(value).map_err(|_| windows::Win32::Foundation::E_INVALIDARG.into())
}

/// Converts Rust bounds to an offset and length without wrapping into CF_EOF (-1).
pub(crate) fn checked_range(range: impl RangeBounds<u64>) -> core::Result<(i64, i64)> {
    let invalid = || core::Error::from(windows::Win32::Foundation::E_INVALIDARG);
    let start = match range.start_bound() {
        Bound::Included(&start) => start,
        Bound::Excluded(&start) => start.checked_add(1).ok_or_else(invalid)?,
        Bound::Unbounded => 0,
    };
    let offset = checked_offset(start)?;
    let end = match range.end_bound() {
        Bound::Included(&end) => end.checked_add(1).ok_or_else(invalid)?,
        Bound::Excluded(&end) => end,
        Bound::Unbounded => return Ok((offset, -1)),
    };
    checked_offset(end)?;
    Ok((
        offset,
        checked_offset(end.checked_sub(start).ok_or_else(invalid)?)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_preserve_offset_and_length() {
        assert_eq!(checked_range(4096..8192).unwrap(), (4096, 4096));
        assert_eq!(checked_range(4096..=8191).unwrap(), (4096, 4096));
        assert_eq!(checked_range(4096..).unwrap(), (4096, -1));
        assert_eq!(checked_range(..0).unwrap(), (0, 0));
        assert_eq!(
            checked_range((Bound::Excluded(4095), Bound::Excluded(8192))).unwrap(),
            (4096, 4096)
        );
    }

    #[test]
    fn invalid_ranges_never_wrap_into_eof() {
        assert!(checked_range((Bound::Included(8192), Bound::Excluded(4096))).is_err());
        assert!(checked_range(..=u64::MAX).is_err());
        assert!(checked_range((Bound::Excluded(u64::MAX), Bound::Unbounded)).is_err());
        assert!(checked_range((i64::MAX as u64 + 1)..).is_err());
    }
}
