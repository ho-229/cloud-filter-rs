//! External Shell client for the shared file-operation scenarios.
use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context};
use widestring::U16CString;
use windows::{
    core::PCWSTR,
    Win32::{
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::Com::*,
        UI::Shell::*,
    },
};

fn item(path: &Path) -> windows::core::Result<IShellItem> {
    let path = U16CString::from_os_str(path).unwrap();
    unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }
}

// The parent supplies an isolated test root and checks the actual filesystem results.
pub fn client() -> anyhow::Result<()> {
    let root = PathBuf::from(env::var_os("CF_PROBE_ROOT").context("test root")?);
    let path = root.join(env::var_os("CF_PATH").context("test path")?);
    let action = env::var("CF_ACTION")?;
    let value = env::var("CF_VALUE").unwrap_or_default();
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let result = (|| unsafe {
        match action.as_str() {
            "list" => {
                let entries: IEnumShellItems = item(&path)?.BindToHandler(None, &BHID_EnumItems)?;
                let mut names = Vec::new();
                loop {
                    let mut next = [None];
                    entries.Next(&mut next, None)?;
                    let Some(entry) = next[0].take() else { break };
                    let name = entry.GetDisplayName(SIGDN_PARENTRELATIVEPARSING)?;
                    let text = name.to_string();
                    CoTaskMemFree(Some(name.0.cast()));
                    names.push(text?);
                }
                names.sort();
                ensure!(names.join(",") == value, "unexpected entries: {names:?}");
            }
            "read" | "write" => {
                // BHID_Stream uses the bind context's STGM flags for read/write access.
                let context = CreateBindCtx(0)?;
                context.SetBindOptions(&BIND_OPTS {
                    cbStruct: size_of::<BIND_OPTS>() as u32,
                    grfMode: (if action == "write" {
                        STGM_READWRITE
                    } else {
                        STGM_READ
                    } | STGM_SHARE_DENY_NONE)
                        .0,
                    ..Default::default()
                })?;
                let stream: IStream = item(&path)?.BindToHandler(&context, &BHID_Stream)?;
                let mut transferred = 0;
                if action == "write" {
                    stream
                        .Write(
                            value.as_ptr().cast(),
                            value.len() as u32,
                            Some(&mut transferred),
                        )
                        .ok()?;
                    ensure!(transferred as usize == value.len(), "short write");
                    stream.SetSize(value.len() as u64)?;
                    stream.Commit(STGC_DEFAULT)?;
                } else {
                    let mut data = vec![0; value.len() + 1];
                    stream
                        .Read(
                            data.as_mut_ptr().cast(),
                            data.len() as u32,
                            Some(&mut transferred),
                        )
                        .ok()?;
                    ensure!(
                        &data[..transferred as usize] == value.as_bytes(),
                        "content mismatch"
                    );
                }
            }
            _ => {
                let operation: IFileOperation =
                    CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
                operation.SetOperationFlags(FOF_NO_UI)?; // Permanent deletion, like Shift+Delete.
                let name = U16CString::from_str(&value)?;
                match action.as_str() {
                    "mkdir" | "create" => {
                        let filename = U16CString::from_os_str(path.file_name().unwrap())?;
                        let attributes = if action == "mkdir" {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        };
                        operation.NewItem(
                            &item(path.parent().unwrap())?,
                            attributes.0,
                            PCWSTR(filename.as_ptr()),
                            PCWSTR::null(),
                            None,
                        )?;
                    }
                    "rename" => operation.RenameItem(&item(&path)?, PCWSTR(name.as_ptr()), None)?,
                    "copy" | "move" => {
                        let target = root.join(&value);
                        let folder = item(target.parent().unwrap())?;
                        let filename = U16CString::from_os_str(target.file_name().unwrap())?;
                        if action == "copy" {
                            operation.CopyItem(
                                &item(&path)?,
                                &folder,
                                PCWSTR(filename.as_ptr()),
                                None,
                            )?;
                        } else {
                            operation.MoveItem(
                                &item(&path)?,
                                &folder,
                                PCWSTR(filename.as_ptr()),
                                None,
                            )?;
                        }
                    }
                    "delete" => operation.DeleteItem(&item(&path)?, None)?,
                    _ => bail!("unknown test action: {action}"),
                }
                operation.PerformOperations()?;
                let aborted = operation.GetAnyOperationsAborted()?.as_bool();
                println!("IFileOperation {action} succeeded, aborted={aborted}");
                ensure!(!aborted, "Shell operation aborted");
            }
        }
        Ok(())
    })();
    unsafe { CoUninitialize() };
    result
}
