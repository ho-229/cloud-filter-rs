#![allow(clippy::missing_safety_doc)]

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Weak},
};

use windows::Win32::Storage::CloudFilters::{
    self, CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS, CF_CALLBACK_REGISTRATION,
};

use crate::{
    command::{self, Fallible},
    error::CloudErrorKind,
    filter::{info, ticket, Request, SyncFilter},
};

pub type Callbacks = [CF_CALLBACK_REGISTRATION; 14];

macro_rules! callbacks {
    ($([$type:path, $name:ident]),*) => {
        [
            $(
                CF_CALLBACK_REGISTRATION {
                    Type: $type,
                    Callback: Some({
                        unsafe extern "system" fn invoke<T: SyncFilter + 'static>(
                            info: *const CF_CALLBACK_INFO,
                            params: *const CF_CALLBACK_PARAMETERS,
                        ) {
                            callback_boundary(info, params, $type, $name::<T>);
                        }
                        invoke::<T>
                    }),
                },
            )*
            CF_CALLBACK_REGISTRATION {
                Type: CloudFilters::CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ]
    };
}

// TODO: const this
pub fn callbacks<T: SyncFilter + 'static>() -> Callbacks {
    callbacks!(
        [CloudFilters::CF_CALLBACK_TYPE_FETCH_DATA, fetch_data],
        [CloudFilters::CF_CALLBACK_TYPE_VALIDATE_DATA, validate_data],
        [
            CloudFilters::CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            cancel_fetch_data
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            fetch_placeholders
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            cancel_fetch_placeholders
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
            notify_file_open_completion
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            notify_file_close_completion
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE,
            notify_dehydrate
        ],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE_COMPLETION,
            notify_dehydrate_completion
        ],
        [CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DELETE, notify_delete],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
            notify_delete_completion
        ],
        [CloudFilters::CF_CALLBACK_TYPE_NOTIFY_RENAME, notify_rename],
        [
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
            notify_rename_completion
        ]
    )
}

unsafe fn fetch_data<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let info = info::FetchData((*params).Anonymous.FetchData);
        let range = info.required_file_range();
        let ticket = ticket::FetchData::new(connection_key, transfer_key, range.clone());

        let Err(e) = filter.fetch_data(request, ticket, info) else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::Write::fail(connection_key, transfer_key, range, e);
    }
}

unsafe fn validate_data<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let info = info::ValidateData((*params).Anonymous.ValidateData);
        let range = info.file_range();
        let ticket = ticket::ValidateData::new(connection_key, transfer_key);

        let Err(e) = filter.validate_data(request, ticket, info) else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::Validate::fail(connection_key, transfer_key, range, e);
    }
}

unsafe fn cancel_fetch_data<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.cancel_fetch_data(
            Request::new(*info),
            info::CancelFetchData((*params).Anonymous.Cancel),
        );
    }
}

unsafe fn fetch_placeholders<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let ticket = ticket::FetchPlaceholders::new(connection_key, transfer_key);

        let Err(e) = filter.fetch_placeholders(
            request,
            ticket,
            info::FetchPlaceholders::new((*params).Anonymous.FetchPlaceholders),
        ) else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::CreatePlaceholders::fail(connection_key, transfer_key, (), e);
    }
}

unsafe fn cancel_fetch_placeholders<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.cancel_fetch_placeholders(
            Request::new(*info),
            info::CancelFetchPlaceholders((*params).Anonymous.Cancel),
        );
    }
}

unsafe fn notify_file_open_completion<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.opened(
            Request::new(*info),
            info::Opened((*params).Anonymous.OpenCompletion),
        );
    }
}

unsafe fn notify_file_close_completion<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.closed(
            Request::new(*info),
            info::Closed((*params).Anonymous.CloseCompletion),
        );
    }
}

unsafe fn notify_dehydrate<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let ticket = ticket::Dehydrate::new(connection_key, transfer_key);

        let Err(e) = filter.dehydrate(
            request,
            ticket,
            info::Dehydrate((*params).Anonymous.Dehydrate),
        ) else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::Dehydrate::fail(connection_key, transfer_key, (), e);
    }
}

unsafe fn notify_dehydrate_completion<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.dehydrated(
            Request::new(*info),
            info::Dehydrated((*params).Anonymous.DehydrateCompletion),
        );
    }
}

unsafe fn notify_delete<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let ticket = ticket::Delete::new(connection_key, transfer_key);

        let Err(e) = filter.delete(request, ticket, info::Delete((*params).Anonymous.Delete))
        else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::Delete::fail(connection_key, transfer_key, (), e);
    }
}

unsafe fn notify_delete_completion<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        filter.deleted(
            Request::new(*info),
            info::Deleted((*params).Anonymous.DeleteCompletion),
        );
    }
}

unsafe fn notify_rename<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let connection_key = request.connection_key();
        let transfer_key = request.transfer_key();
        let ticket = ticket::Rename::new(connection_key, transfer_key);
        let info = info::Rename::new((*params).Anonymous.Rename, request.volume_letter());

        let Err(e) = filter.rename(request, ticket, info) else {
            return;
        };

        // Fails if the request was already cancelled or timed out. Nothing more can be done, and
        // a panic here would abort the process: these callbacks are `extern "system"`.
        let _ = command::Rename::fail(connection_key, transfer_key, (), e);
    }
}

unsafe fn notify_rename_completion<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(filter) = filter_from_info::<T>(info) {
        let request = Request::new(*info);
        let info = info::Renamed::new(
            (*params).Anonymous.RenameCompletion,
            request.volume_letter(),
        );
        filter.renamed(request, info);
    }
}

unsafe fn filter_from_info<T: SyncFilter + 'static>(
    info: *const CF_CALLBACK_INFO,
) -> Option<Arc<T>> {
    // Connection owns this stable Weak slot until CfDisconnectSyncRoot returns.
    ((*info).CallbackContext as *const Weak<T>)
        .as_ref()?
        .upgrade()
}

unsafe fn callback_boundary(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
    kind: CloudFilters::CF_CALLBACK_TYPE,
    callback: unsafe fn(*const CF_CALLBACK_INFO, *const CF_CALLBACK_PARAMETERS),
) {
    if info.is_null() || params.is_null() {
        return;
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| callback(info, params))) {
        // A user-supplied panic payload can itself panic when dropped. Neither panic
        // may unwind through the Windows callback boundary.
        std::mem::forget(payload);
        let connection = (*info).ConnectionKey.0;
        let transfer = (*info).TransferKey;
        let error = CloudErrorKind::Unsuccessful;
        let _ = match kind {
            CloudFilters::CF_CALLBACK_TYPE_FETCH_DATA => command::Write::fail(
                connection,
                transfer,
                info::FetchData((*params).Anonymous.FetchData).required_file_range(),
                error,
            ),
            CloudFilters::CF_CALLBACK_TYPE_VALIDATE_DATA => command::Validate::fail(
                connection,
                transfer,
                info::ValidateData((*params).Anonymous.ValidateData).file_range(),
                error,
            ),
            CloudFilters::CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS => {
                command::CreatePlaceholders::fail(connection, transfer, (), error)
            }
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE => {
                command::Dehydrate::fail(connection, transfer, (), error)
            }
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_DELETE => {
                command::Delete::fail(connection, transfer, (), error)
            }
            CloudFilters::CF_CALLBACK_TYPE_NOTIFY_RENAME => {
                command::Rename::fail(connection, transfer, (), error)
            }
            _ => Ok(()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PanickingFilter(AtomicUsize);
    struct PanickingPayload;
    impl Drop for PanickingPayload {
        fn drop(&mut self) {
            panic!("payload destructor must not cross FFI");
        }
    }
    impl SyncFilter for PanickingFilter {
        fn fetch_data(&self, _: Request, _: ticket::FetchData, _: info::FetchData) -> CResult<()> {
            Ok(())
        }
        fn opened(&self, _: Request, _: info::Opened) {
            self.0.fetch_add(1, Ordering::Relaxed);
            std::panic::panic_any(PanickingPayload);
        }
    }

    #[test]
    fn native_callback_contains_panics_and_preserves_context() {
        let filter = Arc::new(PanickingFilter(AtomicUsize::new(0)));
        let context = Box::new(Arc::downgrade(&filter));
        let info = CF_CALLBACK_INFO {
            CallbackContext: (&*context as *const Weak<PanickingFilter>)
                .cast_mut()
                .cast(),
            ..Default::default()
        };
        let params = CF_CALLBACK_PARAMETERS::default();
        let callback = callbacks::<PanickingFilter>()
            .into_iter()
            .find(|entry| entry.Type == CloudFilters::CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION)
            .unwrap()
            .Callback
            .unwrap();
        for _ in 0..2 {
            unsafe { callback(&info, &params) };
        }
        assert_eq!(filter.0.load(Ordering::Relaxed), 2);
        assert_eq!(Arc::strong_count(&filter), 1);
        assert_eq!(Arc::weak_count(&filter), 1);
        drop(context);
        assert_eq!(Arc::weak_count(&filter), 0);
    }
}
