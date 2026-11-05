use std::{
    mem::{self, offset_of},
    ptr,
};
use windows::{
    core,
    Win32::Storage::CloudFilters::{
        CfExecute, CF_CONNECTION_KEY, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS,
        CF_OPERATION_PARAMETERS_0, CF_OPERATION_TYPE,
    },
};

use crate::{
    error::CloudErrorKind,
    filter::{RawConnectionKey, RawRequestKey, RawTransferKey},
};

/// Allows a command to fail with a specified error.
pub trait Fallible: Command {
    /// Information needed to fail this operation, such as the requested byte range.
    type Context;

    fn fail(
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
        context: Self::Context,
        error_kind: CloudErrorKind,
    ) -> core::Result<Self::Result>;
}

/// A Cloud Filter command used to execute various functions.
pub trait Command: Sized {
    /// The [CF_OPERATION_TYPE] corresponding to the command.
    const OPERATION: CF_OPERATION_TYPE;
    /// The result of the command, if applicable.
    type Result;
    /// The native union field type used to calculate this operation's parameter size.
    type Field;

    /// Gathers and returns the result of the command.
    ///
    /// # Safety
    /// The union member corresponding to `Self::OPERATION` must be initialized,
    /// including any output fields read by this implementation.
    unsafe fn result(info: CF_OPERATION_PARAMETERS_0) -> Self::Result;

    /// Creates and returns the union.
    fn build(&mut self) -> core::Result<CF_OPERATION_PARAMETERS_0>;

    /// Executes the command to the platform.
    fn execute(
        &mut self,
        connection_key: RawConnectionKey,
        transfer_key: RawTransferKey,
        request_key: RawRequestKey,
    ) -> core::Result<Self::Result> {
        execute::<Self>(self.build()?, connection_key, transfer_key, request_key)
    }
}

pub fn execute<C: Command>(
    info: CF_OPERATION_PARAMETERS_0,
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
    request_key: RawRequestKey,
) -> core::Result<C::Result> {
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: (mem::size_of::<C::Field>() + offset_of!(CF_OPERATION_PARAMETERS, Anonymous))
            as u32,
        Anonymous: info,
    };
    unsafe {
        CfExecute(
            &CF_OPERATION_INFO {
                StructSize: mem::size_of::<CF_OPERATION_INFO>() as u32,
                Type: C::OPERATION,
                ConnectionKey: CF_CONNECTION_KEY(connection_key),
                TransferKey: transfer_key,
                CorrelationVector: ptr::null(),
                SyncStatus: ptr::null(),
                RequestKey: request_key,
            } as *const _,
            &mut parameters,
        )?;
        Ok(C::result(parameters.Anonymous))
    }
}
