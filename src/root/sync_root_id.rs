use std::{
    ffi::{OsStr, OsString},
    mem,
    os::windows::ffi::OsStringExt,
    path::Path,
    ptr,
};

use widestring::{u16cstr, U16CStr, U16CString, U16Str, U16String};
use windows::{
    core::{self, Error, HSTRING, PCWSTR, PWSTR},
    Storage::{Provider::StorageProviderSyncRootManager, StorageFolder},
    Win32::{
        Foundation::{
            self, LocalFree, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, HANDLE, HLOCAL,
        },
        Security::{self, Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_USER},
        Storage::CloudFilters,
        System::{
            Com::{self, CoCreateInstance},
            Search::{self, ISearchManager},
        },
    },
};

use crate::utility::{storage_path, ToHString};

use super::SyncRootInfo;

/// Lists roots registered with the WinRT sync-root manager, including disconnected providers.
pub fn active_roots() -> core::Result<Vec<SyncRootInfo>> {
    StorageProviderSyncRootManager::GetCurrentSyncRoots()
        .map(|list| list.into_iter().map(SyncRootInfo).collect())
}

/// Queries support through the WinRT sync-root manager.
/// This does not probe every native CFAPI operation or filesystem.
pub fn is_supported() -> core::Result<bool> {
    StorageProviderSyncRootManager::IsSupported()
}

/// A builder to construct a [SyncRootId].
#[derive(Debug, Clone)]
pub struct SyncRootIdBuilder {
    provider_name: U16String,
    user_security_id: SecurityId,
    account_name: U16String,
}

impl SyncRootIdBuilder {
    /// Create a new builder with the given provider name.
    ///
    /// The builder limits the name to 255 UTF-16 code units and rejects `!` delimiters.
    /// This does not validate the length of the complete registration ID.
    ///
    /// # Panics
    ///
    /// Panics if the provider name exceeds that limit or contains exclamation points.
    pub fn new(provider_name: impl AsRef<OsStr>) -> Self {
        let name = U16String::from_os_str(&provider_name);

        assert!(
            name.len() <= CloudFilters::CF_MAX_PROVIDER_NAME_LENGTH as usize,
            "provider name must not exceed {} characters, got {} characters",
            CloudFilters::CF_MAX_PROVIDER_NAME_LENGTH,
            name.len()
        );
        assert!(
            !name.as_slice().contains(&SyncRootId::SEPARATOR),
            "provider name must not contain exclamation points"
        );

        Self {
            provider_name: name,
            user_security_id: SecurityId(U16String::new()),
            account_name: U16String::new(),
        }
    }

    /// The security id of the Windows user. Retrieve this value via the
    /// [SecurityId] struct.
    ///
    /// The default component is empty; the builder does not infer the current user.
    pub fn user_security_id(mut self, security_id: SecurityId) -> Self {
        self.user_security_id = security_id;
        self
    }

    /// The provider's stable account identifier, used to distinguish sync roots.
    ///
    /// This becomes part of the registration ID, not a display label. Avoid `!`,
    /// which separates components and would make [`SyncRootId::to_components`] fail.
    pub fn account_name(mut self, account_name: impl AsRef<OsStr>) -> Self {
        self.account_name = U16String::from_os_str(&account_name);
        self
    }

    /// Constructs a [SyncRootId] from the builder.
    pub fn build(self) -> SyncRootId {
        SyncRootId(
            [
                self.provider_name.as_slice(),
                self.user_security_id.0.as_slice(),
                self.account_name.as_slice(),
            ]
            .join(&SyncRootId::SEPARATOR)
            .to_hstring(),
        )
    }
}

/// The identifier for a sync root.
///
/// The inner value comes in the form:
/// `provider-id!security-id!account-name`
/// as specified
/// [here](https://docs.microsoft.com/en-us/uwp/api/windows.storage.provider.storageprovidersyncrootinfo.id?view=winrt-22000#property-value).
///
/// A [SyncRootId] stores a reference-counted [HSTRING], making it cheap to clone.
#[derive(Debug, Clone)]
pub struct SyncRootId(pub(crate) HSTRING);

impl SyncRootId {
    // https://docs.microsoft.com/en-us/uwp/api/windows.storage.provider.storageprovidersyncrootinfo.id?view=winrt-22000#windows-storage-provider-storageprovidersyncrootinfo-id
    // unicode exclamation point as told in the specification above
    const SEPARATOR: u16 = 0x21;

    /// Looks up the registered sync-root ID for a folder; does not create a registration.
    pub fn from_path<P: AsRef<Path>>(path: P) -> core::Result<Self> {
        let path = storage_path(path.as_ref())?;
        StorageProviderSyncRootManager::GetSyncRootInformationForFolder(
            &StorageFolder::GetFolderFromPathAsync(&HSTRING::from(path.as_os_str()))?.get()?,
        )
        .and_then(|info| info.Id().map(Self))
    }

    /// Whether or not the [SyncRootId] has already been registered.
    ///
    /// This queries the WinRT/Shell registration. A `false` result means WinRT
    /// returned `ERROR_NOT_FOUND`; it does not independently check the native
    /// sync-root state. WinRT can also return that error for an inaccessible path.
    pub fn is_registered(&self) -> core::Result<bool> {
        match StorageProviderSyncRootManager::GetSyncRootInformationForId(&self.0) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == Foundation::ERROR_NOT_FOUND.to_hresult() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Returns the sync root information for the [SyncRootId].
    pub fn info(&self) -> core::Result<SyncRootInfo> {
        StorageProviderSyncRootManager::GetSyncRootInformationForId(&self.0).map(SyncRootInfo)
    }

    /// Registers the sync root at the current [SyncRootId].
    ///
    /// [SyncRootInfo::display_name], [SyncRootInfo::icon], [SyncRootInfo::version] and [SyncRootInfo::path]
    /// are required and cannot be empty.
    ///
    /// Registration does not convert existing children to placeholders or reconcile
    /// them with remote data. Use [Session::connect][super::Session::connect] to resume
    /// an existing registration; registration updates can change population state.
    pub fn register(&self, info: SyncRootInfo) -> core::Result<()> {
        macro_rules! check_field {
            ($info:ident, $field:ident) => {
                if $info.$field().eq(OsStr::new("")) {
                    Err(Error::new(
                        ERROR_INVALID_PARAMETER.to_hresult(),
                        concat!(stringify!($field), " cannot be empty"),
                    ))?;
                }
            };
        }
        check_field!(info, display_name);
        check_field!(info, icon);
        check_field!(info, version);
        check_field!(info, path);

        info.0.SetId(&self.0).unwrap();
        StorageProviderSyncRootManager::Register(&info.0)
    }

    /// Unregisters the sync root at the current [SyncRootId] if it exists.
    ///
    /// Drop all connections first. This is removal of the sync root, not a way to
    /// pause synchronization: fully hydrated files and fully populated directories
    /// are reverted to ordinary entries; other placeholders can be deleted locally.
    /// Entries that cannot be reverted or deleted may be skipped, so success does
    /// not imply that all placeholder metadata has been removed.
    ///
    /// See [CfUnregisterSyncRoot](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfunregistersyncroot).
    pub fn unregister(&self) -> core::Result<()> {
        StorageProviderSyncRootManager::Unregister(&self.0)
    }

    /// Indexes the sync root at the current [SyncRootId].
    ///
    /// Returns an error if the sync root does not exist or unable to index.
    pub fn index(&self) -> core::Result<()> {
        let path = self.info()?.path();
        index_path(&path)
    }

    /// Encodes the [SyncRootId] to an [OsString].
    pub fn to_os_string(&self) -> OsString {
        OsString::from_wide(self.0.as_wide())
    }

    /// A reference to the [SyncRootId] as a 16 bit string.
    pub fn as_u16_str(&self) -> &U16Str {
        U16Str::from_slice(self.0.as_wide())
    }

    /// Borrows the underlying [HSTRING].
    pub fn as_hstring(&self) -> &HSTRING {
        &self.0
    }

    /// The three components of a [SyncRootId] as described by the specification.
    ///
    /// The order goes as follows:
    /// `(provider-id, security-id, account-name)`
    ///
    /// # Panics
    ///
    /// Panics if the sync root id does not have exactly three components.
    pub fn to_components(&self) -> (&U16Str, &U16Str, &U16Str) {
        let mut components = Vec::with_capacity(3);
        components.extend(
            self.0
                .as_wide()
                .split(|&byte| byte == Self::SEPARATOR)
                .map(U16Str::from_slice),
        );

        if components.len() != 3 {
            panic!("malformed sync root id, got {:?}", components)
        }

        (components[0], components[1], components[2])
    }
}

/// A user security id (SID).
#[derive(Debug, Clone)]
pub struct SecurityId(U16String);

impl SecurityId {
    // https://docs.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentthreadeffectivetoken
    const CURRENT_THREAD_EFFECTIVE_TOKEN: HANDLE = HANDLE(-6isize as *mut ::core::ffi::c_void);

    /// Creates a new [SecurityId] from [OsString].
    ///
    /// # Panics
    ///
    /// Panics if the security id contains an exclamation point.
    pub fn new(id: impl AsRef<OsStr>) -> Self {
        let id = U16String::from_os_str(&id);
        assert!(
            !id.as_slice().contains(&SyncRootId::SEPARATOR),
            "security id cannot contain exclamation points"
        );

        Self(id)
    }

    /// The SID from the calling thread's effective token, including impersonation.
    pub fn current_user() -> core::Result<Self> {
        unsafe {
            let mut token_size = 0;

            // get the token size
            let info = GetTokenInformation(
                Self::CURRENT_THREAD_EFFECTIVE_TOKEN,
                Security::TokenUser,
                None,
                0,
                &mut token_size,
            );

            if let Err(e) = info {
                if e.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult() {
                    Err(e)?;
                }
            }

            // TOKEN_USER and the trailing SID require native alignment.
            let mut buffer = vec![0usize; (token_size as usize).div_ceil(mem::size_of::<usize>())];

            GetTokenInformation(
                Self::CURRENT_THREAD_EFFECTIVE_TOKEN,
                Security::TokenUser,
                Some(buffer.as_mut_ptr() as *mut _),
                token_size,
                &mut token_size,
            )?;

            if (token_size as usize) < mem::size_of::<TOKEN_USER>() {
                return Err(Foundation::E_UNEXPECTED.into());
            }
            let token_user = (buffer.as_ptr() as *const TOKEN_USER).read_unaligned();
            let mut sid = PWSTR(ptr::null_mut());
            ConvertSidToStringSidW(token_user.User.Sid, &mut sid as *mut _)?;

            let string_sid = U16CStr::from_ptr_str(sid.0).to_os_string();
            LocalFree(HLOCAL(sid.0 as *mut _));

            Ok(SecurityId::new(string_sid))
        }
    }
}

fn index_path(path: &Path) -> core::Result<()> {
    unsafe {
        let searcher: ISearchManager = CoCreateInstance(
            &Search::CSearchManager as *const _,
            None,
            Com::CLSCTX_SERVER,
        )?;

        let catalog = searcher.GetCatalog(PCWSTR(u16cstr!("SystemIndex").as_ptr()))?;

        let mut url = OsString::from("file:///");
        url.push(path);

        let crawler = catalog.GetCrawlScopeManager()?;
        crawler.AddDefaultScopeRule(
            PCWSTR(
                U16CString::from_os_str(url)
                    .expect("not contains nul")
                    .as_ptr(),
            ),
            true,
            Search::FF_INDEXCOMPLEXURLS.0 as u32,
        )?;

        crawler.SaveAll()
    }
}
