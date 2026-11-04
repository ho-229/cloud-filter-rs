<div align="center">
  <h1><code>cloud-filter</code></h1>
  <p><strong>Windows Cloud Sync</strong></p>
  <p>
    <a href="https://docs.rs/cloud-filter/"><img src="https://img.shields.io/docsrs/cloud-filter" alt="docs" /></a>
    <a href="https://crates.io/crates/cloud-filter"><img src="https://img.shields.io/crates/v/cloud-filter" alt="crates" /></a>
  </p>
</div>

> Forked from [wincs](https://github.com/ok-nick/wincs). Thanks to
> [ok-nick](https://github.com/ok-nick) for the original implementation.

`cloud-filter` is a Rust wrapper around the [Windows Cloud Files APIs](https://learn.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine),
for building sync providers with on-demand files and File Explorer integration.

## Features

- Sync-root registration, policies, and File Explorer integration.
- Placeholder creation, metadata and content-range queries, hydration, dehydration, and pin state.
- Synchronous and asynchronous provider callbacks, with tickets for transferring data and
  responding to file operations.
- Local change notifications through `state_changed`, provider status, and custom item properties.

## Concepts

- **Sync root:** a registered directory managed by a provider. Registration persists;
  a connection attaches the running provider's callbacks.
- **Placeholder:** an NTFS entry with file metadata and a provider-defined identity. It can
  contain none, some, or all of the file's contents locally.
- **Hydration / dehydration:** downloading missing contents / releasing local contents while
  retaining the placeholder. Pinning requests that a file remain available locally.
- **Synchronization:** `InSync` records the provider's assertion that an entry matches its
  backing store. Local availability is independent of this state. The provider decides when
  to upload edits, discover remote changes, and resolve conflicts.

## Getting started

Build on Windows with the MSVC Rust toolchain and use an NTFS volume for the sync root.

### Try Cloud Mirror

[Cloud Mirror](https://github.com/ho-229/cloud-filter-rs/tree/main/examples/cloud-mirror)
uses a local directory as a stand-in for a remote server.

1. Create two separate directories, such as `C:\cf-demo\server` and `C:\cf-demo\client`.
   Put a few test files in `server`; `client` must be empty for its first registration.
   Neither directory should be inside the other.
2. From the repository, run:

   ```powershell
   cargo run --example cloud-mirror -- C:\cf-demo\server C:\cf-demo\client
   ```

3. Open `client` in File Explorer. Browsing creates placeholders; opening a file fetches its
   contents from `server`. Local edits, renames, and deletions are written back to `server`.
   Add `--readonly` to keep the server directory unchanged.
4. Press Ctrl-C to disconnect. Run the same command to reconnect to the existing root.

To remove the registration after stopping the provider:

```powershell
cargo run --example cloud-mirror -- unregister C:\cf-demo\client
```

Unregistering can remove placeholders whose contents are not fully available locally.
See the [design guide](https://github.com/ho-229/cloud-filter-rs/blob/main/examples/cloud-mirror/DESIGN.md)
for how the example handles synchronization.

### Build a provider

1. Register a directory with `SyncRootId` and `SyncRootInfo`, choosing its identity, display
   information, and hydration/population policies.
2. Implement `SyncFilter`, or the asynchronous `Filter`. Supply directory entries in
   `fetch_placeholders` and file contents in `fetch_data`; use `state_changed` to discover local
   edits for your upload logic.
3. Attach the provider with `Session::connect` or `connect_async` and keep the returned
   `Connection` alive. Dropping it disconnects while preserving the registration and placeholders.

Use `Placeholder` to inspect and update entries. The optional `globs` feature enables
`globset` parsing of directory search patterns. The
[API documentation](https://docs.rs/cloud-filter/) describes published releases;
`cargo doc --open` documents the checked-out version.

## FAQ

### Why CFAPI instead of FUSE, Dokany, or WinFsp?

[Dokan](https://dokan-dev.github.io/) and [WinFsp](https://winfsp.dev/) provide user-mode
filesystems on Windows, including FUSE-compatible APIs. CFAPI works with cached files in an
NTFS sync root and supplies Explorer's cloud-file states and controls. The choice depends on
whether you need a general filesystem implementation or Windows sync-provider integration.

### Can the backing store be local?

Yes; Cloud Mirror demonstrates this. For projecting a fast backing store into a filesystem,
[ProjFS](https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system) is another
option. CFAPI is useful when offline availability, download progress, and cloud-file UI matter.

### Does Microsoft still maintain CFAPI?

Microsoft continues to extend the Cloud Files platform, including
[Windows Search integration](https://learn.microsoft.com/en-us/windows/apps/develop/search/cloud-search-integration).
This does not provide a fix schedule for individual CFAPI issues.

## Development status

Experimental and under active development; the API is still changing and is not production-ready.
Use test data and keep backups while experimenting.

### Known Windows issues

These issues were reproduced with standalone Windows SDK programs, independently of this crate.

- **Batch creation can corrupt placeholder metadata** ([report](https://learn.microsoft.com/en-us/answers/questions/6057324/cfapi-cfcreateplaceholders-returns-s-ok-but-create)).
  A native batch can report success for every entry while subsequent access fails with
  `ERROR_CLOUD_FILE_METADATA_CORRUPT` (363). Reproduced on Windows 10 build 19044.7807,
  with an independent reproduction reported on Windows 11 build 26300. Sorting entries by
  identity length or using equal-length identities does not reliably avoid it. The current
  implementation submits one placeholder per native call for both `BatchCreate` and callback
  population. This avoids the observed trigger but does not repair existing corruption.
- **Explorer deletion can leave empty directory hierarchies** ([report](https://learn.microsoft.com/en-us/answers/questions/6060921/cfapi-shell-ifileoperation-returns-s-ok-but-leaves)).
  On Windows 10 build 19044.7807, Shift+Delete and `IFileOperation` can leave empty ancestors
  when deleting populated cloud directories containing offline files under a Shell-registered
  sync root. The Shell operation reports success without cancellation. In our tests, recursive
  filesystem deletion (such as PowerShell `Remove-Item -LiteralPath ... -Recurse`) or fully
  hydrating the files before Shell deletion removed the entire tree.

## Additional Resources

- [Cloud Mirror design](https://github.com/ho-229/cloud-filter-rs/blob/main/examples/cloud-mirror/DESIGN.md)
  and [SFTP design](https://github.com/ho-229/cloud-filter-rs/blob/main/examples/sftp/DESIGN.md)
- [CFAPI behavior notes](https://github.com/ho-229/cloud-filter-rs/blob/main/tests/behavior/lifecycle.md)
- [Cloud Filter API reference](https://learn.microsoft.com/en-us/windows/win32/cfapi/cloud-filter-reference)
- [Windows.Storage.Provider reference](https://learn.microsoft.com/en-us/uwp/api/windows.storage.provider)

## License

[MIT](https://github.com/ho-229/cloud-filter-rs/blob/main/LICENSE).
