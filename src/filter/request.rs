use std::{ffi::OsString, path::PathBuf, slice};

use widestring::U16CStr;
use windows::Win32::Storage::CloudFilters::{CF_CALLBACK_INFO, CF_PROCESS_INFO};

pub type RawConnectionKey = i64;
pub type RawTransferKey = i64;

/// A struct containing standard information for the current file operation.
pub struct Request {
    volume_guid: OsString,
    volume_letter: OsString,
    volume_serial: u32,
    process: Option<Process>,
    sync_root_file_id: i64,
    file_id: i64,
    file_size: u64,
    path: PathBuf,
    priority_hint: u8,
    file_blob: Vec<u8>,
    register_blob: Vec<u8>,
    connection_key: RawConnectionKey,
    transfer_key: RawTransferKey,
}

impl Request {
    /// Copies callback data while all platform-owned pointers are still valid.
    ///
    /// # Safety
    /// All non-null pointers in `info` must be readable for their documented lengths.
    pub(crate) unsafe fn new(info: CF_CALLBACK_INFO) -> Self {
        let volume_letter = wide_string(info.VolumeDosName.0).unwrap_or_default();
        let mut path = PathBuf::from(&volume_letter);
        path.push(wide_string(info.NormalizedPath.0).unwrap_or_default());
        Self {
            volume_guid: wide_string(info.VolumeGuidName.0).unwrap_or_default(),
            volume_letter,
            volume_serial: info.VolumeSerialNumber,
            process: info.ProcessInfo.as_ref().map(|info| Process::new(*info)),
            sync_root_file_id: info.SyncRootFileId,
            file_id: info.FileId,
            file_size: info.FileSize as u64,
            path,
            priority_hint: info.PriorityHint,
            file_blob: copy_blob(info.FileIdentity.cast(), info.FileIdentityLength),
            register_blob: copy_blob(info.SyncRootIdentity.cast(), info.SyncRootIdentityLength),
            connection_key: info.ConnectionKey.0,
            transfer_key: info.TransferKey,
        }
    }

    /// The GUID path of the current volume.
    ///
    /// The returned value comes in the form `\?\Volume{GUID}`.
    pub fn volume_guid_path(&self) -> OsString {
        self.volume_guid.clone()
    }

    /// The letter of the current volume.
    ///
    /// The returned value comes in the form `X:`, where `X` is the drive letter.
    pub fn volume_letter(&self) -> OsString {
        self.volume_letter.clone()
    }

    /// The serial number of the current volume.
    pub fn volume_serial_number(&self) -> u32 {
        self.volume_serial
    }

    /// Information of the user process that triggered the callback.
    ///
    /// The platform gives none for some callbacks, such as a cancelled fetch: then every field
    /// is empty, zero or [None][std::option::Option::None]. Use [Request::try_process] to tell
    /// that apart.
    pub fn process(&self) -> Process {
        self.try_process().unwrap_or_default()
    }

    /// Information of the user process that triggered the callback, or
    /// [None][std::option::Option::None] if the platform gave none, as it does for some
    /// callbacks, such as a cancelled fetch.
    pub fn try_process(&self) -> Option<Process> {
        self.process.clone()
    }

    /// The NTFS file ID of the sync root folder under which the placeholder being operated on
    /// resides.
    pub fn sync_root_file_id(&self) -> i64 {
        self.sync_root_file_id
    }

    /// The NTFS file ID of the placeholder file/directory.
    pub fn file_id(&self) -> i64 {
        self.file_id
    }

    /// The logical size of the placeholder file.
    ///
    /// If the placeholder is a directory, this value will always equal 0.
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    // For now this should be cached on creation
    /// The absolute path of the placeholder file/directory starting from the root directory of the
    /// volume.
    ///
    /// [Read here for more information on this
    /// function.](https://docs.microsoft.com/en-us/windows/win32/api/cfapi/ns-cfapi-cf_callback_info#remarks)
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// A numeric scale ranging from
    /// 0-[15](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/CloudFilters/constant.CF_MAX_PRIORITY_HINT.html)
    /// to describe the priority of the file operation.
    ///
    /// [Currently, this value does not
    /// change.](https://docs.microsoft.com/en-us/answers/questions/798674/priority-in-cf-callback-info.html)
    pub fn priority_hint(&self) -> u8 {
        self.priority_hint
    }

    // https://docs.microsoft.com/en-us/answers/questions/749979/what-is-a-requestkey-cfapi.html
    // pub fn request_key(&self) -> i64 {
    //     self.0.RequestKey
    // }

    /// The byte slice assigned to the current placeholder file/directory.
    pub fn file_blob(&self) -> &[u8] {
        &self.file_blob
    }

    /// The byte slice assigned to the current sync root on registration.
    pub fn register_blob(&self) -> &[u8] {
        &self.register_blob
    }

    // // https://docs.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_callback_type#remarks
    // // after 60 seconds of no report, windows will cancel the request with an error,
    // // this function is a "hack" to avoid the timeout
    // // https://docs.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfexecute#remarks
    // // CfExecute will reset any timers as stated
    // /// By default, the operating system will invalidate the callback after 60 seconds of no
    // /// activity (meaning, no placeholder methods are invoked). If you are prone to this issue,
    // /// consider calling this method or call placeholder methods more frequently.
    // pub fn reset_timeout() {}

    /// A raw connection key used to identify the connection.
    pub(crate) fn connection_key(&self) -> RawConnectionKey {
        self.connection_key
    }

    /// A raw transfer key used to identify the current file operation.
    pub(crate) fn transfer_key(&self) -> RawTransferKey {
        self.transfer_key
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("volume_guid_path", &self.volume_guid_path())
            .field("volume_letter", &self.volume_letter())
            .field("volume_serial_number", &self.volume_serial_number())
            .field("process", &self.process())
            .field("sync_root_file_id", &self.sync_root_file_id())
            .field("file_id", &self.file_id())
            .field("file_size", &self.file_size())
            .field("path", &self.path())
            .field("priority_hint", &self.priority_hint())
            .finish()
    }
}

/// Information about the calling process.
#[derive(Clone, Default)]
pub struct Process {
    name: OsString,
    id: u32,
    session_id: u32,
    application_id: OsString,
    command_line: Option<OsString>,
    path: Option<PathBuf>,
}

impl Process {
    unsafe fn new(info: CF_PROCESS_INFO) -> Self {
        Self {
            name: wide_string(info.PackageName.0).unwrap_or_default(),
            id: info.ProcessId,
            session_id: info.SessionId,
            application_id: wide_string(info.ApplicationId.0).unwrap_or_default(),
            command_line: wide_string(info.CommandLine.0).filter(|value| value != "UNKNOWN"),
            path: wide_string(info.ImagePath.0)
                .filter(|value| value != "UNKNOWN")
                .map(PathBuf::from),
        }
    }

    /// The application's package name.
    pub fn name(&self) -> OsString {
        self.name.clone()
    }

    /// The ID of the user process.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The ID of the session where the user process resides.
    ///
    /// ## Note
    ///
    /// [Process::session_id] is valid in versions 1803 and later.
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// The application's ID.
    pub fn application_id(&self) -> OsString {
        self.application_id.clone()
    }

    /// The exact command used to initialize the user process.
    ///
    /// ## Note
    ///
    /// [Process::command_line] is valid in versions 1803 and later.
    pub fn command_line(&self) -> Option<OsString> {
        self.command_line.clone()
    }

    // TODO: Could be optimized
    /// The absolute path to the main executable file of the process in the format of an NT path.
    ///
    /// This function returns [None][std::option::Option::None] when the operating system failed to
    /// retrieve the path.
    pub fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }
}

/// The NUL-terminated wide string at `ptr`, or [None][std::option::Option::None] if it is null.
pub(crate) unsafe fn wide_string(ptr: *const u16) -> Option<OsString> {
    (!ptr.is_null()).then(|| unsafe { U16CStr::from_ptr_str(ptr) }.to_os_string())
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("name", &self.name())
            .field("id", &self.id())
            .field("session_id", &self.session_id())
            .field("application_id", &self.application_id())
            .field("command_line", &self.command_line())
            .field("path", &self.path())
            .finish()
    }
}

// A null pointer is permitted for an absent identity; never form even an empty slice from it.
unsafe fn copy_blob(ptr: *const u8, len: u32) -> Vec<u8> {
    if len == 0 || ptr.is_null() {
        Vec::new()
    } else {
        slice::from_raw_parts(ptr, len as usize).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use widestring::U16CString;
    use windows::core::PCWSTR;

    #[test]
    fn request_and_process_own_callback_memory() {
        let request = {
            let volume = U16CString::from_str("C:").unwrap();
            let path = U16CString::from_str(r"\root\file").unwrap();
            let image = U16CString::from_str(r"C:\app.exe").unwrap();
            let command = U16CString::from_str("app.exe --test").unwrap();
            let mut identity = [1u8, 2, 3];
            let process = CF_PROCESS_INFO {
                ProcessId: 42,
                ImagePath: PCWSTR(image.as_ptr()),
                CommandLine: PCWSTR(command.as_ptr()),
                ..Default::default()
            };
            let request = unsafe {
                Request::new(CF_CALLBACK_INFO {
                    VolumeDosName: PCWSTR(volume.as_ptr()),
                    NormalizedPath: PCWSTR(path.as_ptr()),
                    ProcessInfo: &process as *const _ as *mut _,
                    FileIdentity: identity.as_ptr().cast(),
                    FileIdentityLength: identity.len() as u32,
                    ..Default::default()
                })
            };
            identity.fill(0);
            assert_eq!(request.file_blob(), &[1, 2, 3]);
            request
        };
        assert_eq!(request.path(), PathBuf::from(r"C:\root\file"));
        assert_eq!(request.file_blob(), &[1, 2, 3]);
        assert!(request.register_blob().is_empty());
        let process = request.process();
        drop(request);
        assert_eq!(process.id(), 42);
        assert_eq!(process.command_line(), Some("app.exe --test".into()));
        assert_eq!(process.path(), Some(PathBuf::from(r"C:\app.exe")));
    }

    #[test]
    fn absent_callback_fields_are_safe_to_inspect() {
        let request = unsafe { Request::new(CF_CALLBACK_INFO::default()) };
        assert!(request.file_blob().is_empty());
        assert!(request.register_blob().is_empty());
        assert!(request.try_process().is_none());
        assert!(request.process().name().is_empty());
        assert!(request.process().path().is_none());
        let _ = format!("{request:?}");
    }
}
