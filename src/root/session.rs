use std::{
    ffi::OsString,
    path::Path,
    sync::{Arc, Weak},
};

use windows::{
    core,
    Win32::{
        Storage::CloudFilters::{self, CfConnectSyncRoot, CF_CONNECT_FLAGS},
        System::{
            Com::{self, CoCreateInstance, CoInitializeEx, COINIT_MULTITHREADED},
            Search::{self, ISearchCatalogManager, ISearchManager},
        },
    },
};

use crate::{
    filter::{self, SyncFilter},
    root::connect::Connection,
};

/// A builder to create a new connection for the sync root at the specified path.
#[derive(Debug, Clone, Copy)]
pub struct Session(CF_CONNECT_FLAGS);

impl Session {
    /// Create a new [Session][crate::Session].
    pub fn new() -> Self {
        unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) }.unwrap();
        Self::default()
    }

    // TODO: what specifically causes an implicit hydration?
    /// The [block_implicit_hydration][crate::Session::block_implicit_hydration] flag will prevent
    /// implicit placeholder hydrations from invoking
    /// [SyncFilter::fetch_data][crate::SyncFilter::fetch_data]. This could occur when an
    /// anti-virus is scanning file system activity on files within the sync root.
    ///
    /// A call to the [FileExt::hydrate][crate::ext::FileExt::hydrate] trait will not be blocked by this flag.
    pub fn block_implicit_hydration(mut self) -> Self {
        self.0 |= CloudFilters::CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION;
        self
    }

    /// Initiates a connection to the sync root with the given [SyncFilter][crate::SyncFilter].
    pub fn connect<P, T>(self, path: P, filter: T) -> core::Result<Connection<Arc<T>>>
    where
        P: AsRef<Path>,
        T: SyncFilter + 'static,
    {
        // https://github.com/microsoft/Windows-classic-samples/blob/27ffb0811ca761741502feaefdb591aebf592193/Samples/CloudMirror/CloudMirror/Utilities.cpp#L19
        index_path(path.as_ref())?;

        let filter = Arc::new(filter);
        let callbacks = filter::callbacks::<T>();
        unsafe {
            CfConnectSyncRoot(
                path.as_ref().as_os_str(),
                callbacks.as_ptr(),
                // create a weak arc so that it could be upgraded when it's being used and when the
                // connection is closed, the filter could be freed
                Weak::into_raw(Arc::downgrade(&filter)) as *const _,
                // This is enabled by default to remove the Option requirement around various fields of the
                // [Request][crate::Request] struct
                self.0
                    | CloudFilters::CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH
                    | CloudFilters::CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO,
            )
        }
        .map(|key| Connection::new(key.0, callbacks, filter))
    }
}

impl Default for Session {
    fn default() -> Self {
        Self(CloudFilters::CF_CONNECT_FLAG_NONE)
    }
}

fn index_path(path: &Path) -> core::Result<()> {
    unsafe {
        let searcher: ISearchManager = CoCreateInstance(
            &Search::CSearchManager as *const _,
            None,
            Com::CLSCTX_SERVER,
        )?;

        let catalog: ISearchCatalogManager = searcher.GetCatalog("SystemIndex")?;

        let mut url = OsString::from("file:///");
        url.push(path);

        let crawler = catalog.GetCrawlScopeManager()?;
        crawler.AddDefaultScopeRule(url, true, Search::FF_INDEXCOMPLEXURLS.0 as u32)?;
        crawler.SaveAll()
    }
}
