use crate::{
    filter::{self, AsyncBridge, Filter, SyncFilter},
    root::{connect::Connection, state_watcher::spawn_root_watcher},
    utility::LocalBoxFuture,
};
use std::{
    path::Path,
    sync::{Arc, Weak},
};
use widestring::U16CString;
use windows::{
    core::{self, PCWSTR},
    Win32::Storage::CloudFilters::{self, CfConnectSyncRoot, CF_CONNECT_FLAGS},
};

/// A builder to create a new connection for the sync root at the specified path.
#[derive(Debug, Clone, Copy)]
pub struct Session(CF_CONNECT_FLAGS);

impl Session {
    /// Create a new [Session].
    pub fn new() -> Self {
        Self::default()
    }

    /// The [Session::block_implicit_hydration] flag will prevent
    /// implicit placeholder hydrations from invoking
    /// [SyncFilter::fetch_data][crate::filter::SyncFilter::fetch_data]. This could occur when an
    /// anti-virus is scanning file system activity on files within the sync root.
    ///
    /// A call to the [Placeholder::hydrate][crate::placeholder::Placeholder::hydrate] trait will not be blocked by this flag.
    pub fn block_implicit_hydration(mut self) -> Self {
        self.0 |= CloudFilters::CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION;
        self
    }

    /// Initiates a connection to the sync root with the given [SyncFilter].
    ///
    /// Fails if the attribute watcher cannot start. Later watcher failures are
    /// delivered through [filter::StateWatchEvent::Stopped]; disconnect is silent.
    pub fn connect<P, F>(self, path: P, filter: F) -> core::Result<Connection<F>>
    where
        P: AsRef<Path>,
        F: SyncFilter + 'static,
    {
        let filter = Arc::new(filter);
        let callbacks = filter::callbacks::<F>();
        let context = Box::new(Arc::downgrade(&filter));
        // The worker reports startup errors before connect succeeds, then waits
        // for the native connection before delivering events.
        let (cancel_token, join_handle) =
            spawn_root_watcher(path.as_ref().to_path_buf(), filter.clone())?;
        let key = unsafe {
            CfConnectSyncRoot(
                PCWSTR(
                    U16CString::from_os_str(path.as_ref())
                        .expect("not contains nul")
                        .as_ptr(),
                ),
                callbacks.as_ptr(),
                // The boxed Weak slot stays at a stable address until disconnect completes.
                Some((&*context as *const Weak<F>).cast()),
                // This is enabled by default to remove the Option requirement around various fields of the
                // [Request][crate::Request] struct
                self.0
                    | CloudFilters::CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH
                    | CloudFilters::CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
            )
        };
        let key = match key {
            Ok(key) => key,
            Err(error) => {
                drop(cancel_token);
                let _ = join_handle.join();
                return Err(error);
            }
        };
        let _ = cancel_token.send(());

        let mut connection = Connection::new(key.0, cancel_token, callbacks, filter, context);
        connection.set_worker(join_handle);
        Ok(connection)
    }

    /// Initiates a connection to the sync root with the given [Filter].
    pub fn connect_async<P, F, B>(
        self,
        path: P,
        filter: F,
        block_on: B,
    ) -> core::Result<Connection<AsyncBridge<F, B>>>
    where
        P: AsRef<Path>,
        F: Filter + 'static,
        B: Fn(LocalBoxFuture<'_, ()>) + Send + Sync + 'static,
    {
        self.connect(path, AsyncBridge::new(filter, block_on))
    }
}

impl Default for Session {
    fn default() -> Self {
        Self(CloudFilters::CF_CONNECT_FLAG_NONE)
    }
}
