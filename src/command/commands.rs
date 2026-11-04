use std::{
    ops::{Bound, Range},
    ptr,
};

use windows::{
    core,
    Win32::{
        Foundation,
        Storage::CloudFilters::{
            self, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0,
            CF_OPERATION_PARAMETERS_0_1, CF_OPERATION_PARAMETERS_0_2, CF_OPERATION_PARAMETERS_0_3,
            CF_OPERATION_PARAMETERS_0_4, CF_OPERATION_PARAMETERS_0_5, CF_OPERATION_PARAMETERS_0_6,
            CF_OPERATION_PARAMETERS_0_7, CF_OPERATION_TYPE,
        },
    },
};

use crate::{
    command::executor::{execute, Command, Fallible},
    error::CloudErrorKind,
    filter::{RawConnectionKey, RawRequestKey, RawTransferKey},
    metadata::Metadata,
    placeholder_create_info::{create_individually, PlaceholderCreateInfo},
    utility,
};

/// Read data from a placeholder file.
#[derive(Debug)]
pub struct Read<'a> {
    /// The buffer of data to read into.
    pub buffer: &'a mut [u8],
    /// The byte offset at which to read.
    pub position: u64,
}

impl Command for Read<'_> {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_RETRIEVE_DATA;

    type Result = u64;
    type Field = CF_OPERATION_PARAMETERS_0_5;

    unsafe fn result(info: CF_OPERATION_PARAMETERS_0) -> Self::Result {
        info.RetrieveData.ReturnedLength as u64
    }

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            RetrieveData: CF_OPERATION_PARAMETERS_0_5 {
                Flags: CloudFilters::CF_OPERATION_RETRIEVE_DATA_FLAG_NONE,
                Buffer: self.buffer.as_mut_ptr().cast(),
                Offset: utility::checked_offset(self.position)?,
                Length: self.buffer.len() as i64,
                ReturnedLength: 0,
            },
        })
    }
}

/// Write data to a placeholder file.
#[derive(Debug)]
pub struct Write<'a> {
    /// The buffer of data to write into the file.
    pub buffer: &'a [u8],
    /// The byte offset at which to write.
    pub position: u64,
}

impl Command for Write<'_> {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_TRANSFER_DATA;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_6;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_6 {
                Flags: CloudFilters::CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
                Buffer: self.buffer.as_ptr() as *mut _,
                Offset: utility::checked_offset(self.position)?,
                Length: self.buffer.len() as i64,
            },
        })
    }
}

impl Fallible for Write<'_> {
    type Context = Range<u64>;

    /// Fails the requested range supplied in `context`. A zero-length failure can
    /// be rejected by Windows, leaving the reader waiting for the request timeout.
    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<()> {
        let (offset, length) = utility::checked_range(context)?;
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                TransferData: CF_OPERATION_PARAMETERS_0_6 {
                    Flags: CloudFilters::CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                    Buffer: std::ptr::null_mut(),
                    Offset: offset,
                    Length: length,
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

/// Discard resident data and restart pending hydration with optional new metadata.
#[derive(Debug)]
pub struct RestartHydration<'a> {
    /// Current size reported by the callback, used to reject unsupported resizing.
    pub file_size: u64,
    /// Whether or not to mark the placeholder as "synced."
    pub mark_in_sync: bool,
    /// Optional metadata to update.
    pub metadata: Option<Metadata>,
    /// Optional file blob to update.
    pub blob: &'a [u8],
}

impl Command for RestartHydration<'_> {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_RESTART_HYDRATION;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_4;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        // Resizing pending hydration stalled in CldFlt on Windows 10 build 19044.
        if self
            .metadata
            .is_some_and(|metadata| metadata.0.FileSize as u64 != self.file_size)
        {
            return Err(Foundation::ERROR_NOT_SUPPORTED.into());
        }
        Ok(CF_OPERATION_PARAMETERS_0 {
            RestartHydration: CF_OPERATION_PARAMETERS_0_4 {
                Flags: if self.mark_in_sync {
                    CloudFilters::CF_OPERATION_RESTART_HYDRATION_FLAG_MARK_IN_SYNC
                } else {
                    CloudFilters::CF_OPERATION_RESTART_HYDRATION_FLAG_NONE
                },
                FsMetadata: self
                    .metadata
                    .as_ref()
                    .map_or(ptr::null(), |metadata| &metadata.0 as *const _),
                FileIdentity: match self.blob.is_empty() {
                    true => ptr::null(),
                    false => self.blob.as_ptr() as *const _,
                },
                FileIdentityLength: checked_blob_len(self.blob)?,
            },
        })
    }
}

/// Create placeholder files/directories.
#[derive(Debug)]
pub struct CreatePlaceholders<'a> {
    /// The placeholders to create.
    pub placeholders: &'a mut [PlaceholderCreateInfo],
    /// A total greater than all entries submitted so far, keeping population pending.
    pub total: u64,
}

impl CreatePlaceholders<'_> {
    pub fn complete(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        total: u64,
    ) -> core::Result<()> {
        let mut parameters = CreatePlaceholders {
            placeholders: &mut [],
            total,
        }
        .build()?;
        parameters.TransferPlaceholders.Flags =
            CloudFilters::CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION;
        execute::<Self>(parameters, connection_key, transfer_key, request_key)
    }
}

impl Command for CreatePlaceholders<'_> {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_7;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn execute(
        &mut self,
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
    ) -> core::Result<Self::Result> {
        let mut parameters = self.build()?;
        parameters.TransferPlaceholders.PlaceholderCount = 1;
        create_individually(self.placeholders, |entry| {
            parameters.TransferPlaceholders.PlaceholderArray = entry;
            execute::<Self>(parameters, connection_key, transfer_key, request_key)
        })
    }

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: CF_OPERATION_PARAMETERS_0_7 {
                Flags: CloudFilters::CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
                PlaceholderTotalCount: utility::checked_offset(self.total)?,
                PlaceholderArray: match self.placeholders.is_empty() {
                    // Empty transfers use NULL, not an empty slice's dangling pointer.
                    true => ptr::null_mut(),
                    false => self.placeholders.as_mut_ptr().cast(),
                },
                PlaceholderCount: self
                    .placeholders
                    .len()
                    .try_into()
                    .map_err(|_| core::Error::from(Foundation::E_INVALIDARG))?,
                EntriesProcessed: 0,
            },
        })
    }
}

impl Fallible for CreatePlaceholders<'_> {
    type Context = ();

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        _context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<Self::Result> {
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                TransferPlaceholders: CF_OPERATION_PARAMETERS_0_7 {
                    Flags: CloudFilters::CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                    PlaceholderTotalCount: 0,
                    PlaceholderArray: ptr::null_mut(),
                    PlaceholderCount: 0,
                    EntriesProcessed: 0,
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

/// Acknowledge a data range that the provider has validated.
#[derive(Debug)]
pub struct Validate {
    /// The range of data to validate as "good."
    pub range: (Bound<u64>, Bound<u64>),
}

impl Command for Validate {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_ACK_DATA;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_0;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        let (offset, length) = utility::checked_range(self.range)?;
        Ok(CF_OPERATION_PARAMETERS_0 {
            AckData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CloudFilters::CF_OPERATION_ACK_DATA_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
                Offset: offset,
                Length: length,
            },
        })
    }
}

impl Fallible for Validate {
    type Context = Range<u64>;

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<()> {
        let (offset, length) = utility::checked_range(context)?;
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                AckData: CF_OPERATION_PARAMETERS_0_0 {
                    Flags: CloudFilters::CF_OPERATION_ACK_DATA_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                    Offset: offset,
                    Length: length,
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

/// Permit a pending dehydration and optionally replace the file identity.
#[derive(Debug)]
pub struct Dehydrate<'a> {
    /// Optional file blob to update.
    pub blob: &'a [u8],
}

impl Command for Dehydrate<'_> {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_ACK_DEHYDRATE;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_1;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            AckDehydrate: CF_OPERATION_PARAMETERS_0_1 {
                Flags: CloudFilters::CF_OPERATION_ACK_DEHYDRATE_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
                FileIdentity: match self.blob.is_empty() {
                    true => ptr::null(),
                    false => self.blob.as_ptr() as *const _,
                },
                FileIdentityLength: checked_blob_len(self.blob)?,
            },
        })
    }
}

impl Fallible for Dehydrate<'_> {
    type Context = ();

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        _context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<Self::Result> {
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                AckDehydrate: CF_OPERATION_PARAMETERS_0_1 {
                    Flags: CloudFilters::CF_OPERATION_ACK_DEHYDRATE_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                    FileIdentity: ptr::null(),
                    FileIdentityLength: 0,
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

/// Permit a pending deletion or undelete.
#[derive(Debug)]
pub struct Delete;

impl Command for Delete {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_ACK_DELETE;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_2;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            AckDelete: CF_OPERATION_PARAMETERS_0_2 {
                Flags: CloudFilters::CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
            },
        })
    }
}

impl Fallible for Delete {
    type Context = ();

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        _context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<Self::Result> {
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                AckDelete: CF_OPERATION_PARAMETERS_0_2 {
                    Flags: CloudFilters::CF_OPERATION_ACK_DELETE_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

/// Permit a pending rename or move.
#[derive(Debug)]
pub struct Rename;

impl Command for Rename {
    const OPERATION: CF_OPERATION_TYPE = CloudFilters::CF_OPERATION_TYPE_ACK_RENAME;

    type Result = ();
    type Field = CF_OPERATION_PARAMETERS_0_3;

    unsafe fn result(_info: CF_OPERATION_PARAMETERS_0) -> Self::Result {}

    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0> {
        Ok(CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_3 {
                Flags: CloudFilters::CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: Foundation::STATUS_SUCCESS,
            },
        })
    }
}

impl Fallible for Rename {
    type Context = ();

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        _context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<Self::Result> {
        execute::<Self>(
            CF_OPERATION_PARAMETERS_0 {
                AckRename: CF_OPERATION_PARAMETERS_0_3 {
                    Flags: CloudFilters::CF_OPERATION_ACK_RENAME_FLAG_NONE,
                    CompletionStatus: error_kind.into(),
                },
            },
            connection_key,
            transfer_key,
            request_key,
        )
    }
}

fn checked_blob_len(blob: &[u8]) -> core::Result<u32> {
    if blob.len() > CloudFilters::CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize {
        return Err(Foundation::E_INVALIDARG.into());
    }
    Ok(blob.len() as u32)
}
