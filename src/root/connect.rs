use std::{
    sync::{mpsc::Sender, Arc, Weak},
    thread::{self, JoinHandle},
};

use windows::{
    core,
    Win32::Storage::CloudFilters::{
        CfDisconnectSyncRoot, CfQuerySyncProviderStatus, CfUpdateSyncProviderStatus,
        CF_CONNECTION_KEY,
    },
};

use crate::{
    ext::ProviderStatus,
    filter::{Callbacks, RawConnectionKey},
};

/// A handle to the current session for a given sync root.
///
/// [Connection] will disconnect when dropped. Note that this
/// does **NOT** mean the sync root will be unregistered. To do so, call
/// [SyncRootId::unregister][crate::root::SyncRootId::unregister].
#[derive(Debug)]
pub struct Connection<F> {
    connection_key: RawConnectionKey,

    cancel_token: Sender<()>,
    join_handle: Option<JoinHandle<()>>,

    _callbacks: Callbacks,
    filter: Arc<F>,
    // Windows stores a pointer to the Weak slot, not its allocation.
    #[allow(clippy::redundant_allocation)]
    context: Option<Box<Weak<F>>>,
}

impl<T> Connection<T> {
    #[allow(clippy::redundant_allocation)]
    pub(crate) fn new(
        connection_key: RawConnectionKey,
        cancel_token: Sender<()>,
        callbacks: Callbacks,
        filter: Arc<T>,
        context: Box<Weak<T>>,
    ) -> Self {
        Self {
            connection_key,
            cancel_token,
            join_handle: None,
            _callbacks: callbacks,
            filter,
            context: Some(context),
        }
    }

    pub(crate) fn set_worker(&mut self, worker: JoinHandle<()>) {
        self.join_handle = Some(worker);
    }

    /// A raw connection key used to identify the connection.
    pub fn connection_key(&self) -> RawConnectionKey {
        self.connection_key
    }

    /// A reference to the inner [SyncFilter][crate::filter::SyncFilter] struct.
    pub fn filter(&self) -> &T {
        &self.filter
    }

    /// Queries the status of this provider connection.
    pub fn status(&self) -> core::Result<ProviderStatus> {
        unsafe { CfQuerySyncProviderStatus(CF_CONNECTION_KEY(self.connection_key)) }.map(Into::into)
    }

    /// Updates the provider status queried by [`Self::status`].
    ///
    /// The status lasts for this connection. It does not mark individual placeholders
    /// in sync or initiate any transfers.
    pub fn set_status(&self, status: ProviderStatus) -> core::Result<()> {
        unsafe { CfUpdateSyncProviderStatus(CF_CONNECTION_KEY(self.connection_key), status.into()) }
    }
}

impl<T> Drop for Connection<T> {
    fn drop(&mut self) {
        let disconnected = unsafe { CfDisconnectSyncRoot(CF_CONNECTION_KEY(self.connection_key)) };
        if let Some(context) = self.context.take() {
            if disconnected.is_err() {
                // The platform may still call back after a failed disconnect.
                Box::leak(context);
            }
        }

        _ = self.cancel_token.send(());
        if let Some(worker) = self.join_handle.take() {
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}
