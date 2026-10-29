# Registration lifecycle and callback observations

Registration persists across provider restarts; a connection installs the running provider's callbacks. Normal startup/shutdown should use connect/disconnect. Use registration updates for actual policy changes, and unregister only when removing a sync relationship. The native unregister operation walks the tree, reverts fully hydrated files / fully populated directories, deletes other placeholders, and can skip entries it cannot process. See [CfUnregisterSyncRoot](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfunregistersyncroot).

The probes in [lifecycle.rs](lifecycle.rs) were run on Windows 10 IoT Enterprise LTSC 2021, build **19044.7807**, on NTFS (2026-10-28). These are observations from that build, not additional Windows API guarantees:

* Registering a nonempty directory succeeds. Existing ordinary children remain ordinary; they do not acquire identity or sync state. An existing ordinary subdirectory produces no population callback until converted with on-demand population enabled.
* Disconnect/reconnect preserves placeholders. Updating the existing registration preserves child identity and data, but resets the root to partially populated on this host. Avoid re-registering unchanged settings on each startup.
* Unregister while connected returns `0x8007017C`. After disconnect, unregister deletes both an untouched 8 KiB placeholder and one with only its first 4 KiB hydrated. A fully hydrated file survives as an ordinary file with its contents intact.
* A placeholder held with a read handle opened at the reparse point and denying sharing survives unregister. Registering again therefore produces a **mixed tree**: ordinary survivors and old placeholders with retained identities. Success is not proof that cleanup removed every placeholder.
* Recreating a surviving ordinary file with `PlaceholderCreateInfo` fails on the existing name; it is not automatic reconciliation. Existing ordinary files need an explicit upload/compare/conflict decision before conversion and marking in sync. Existing directories need conversion if their remote children should be populated on demand. Never blindly mark the surviving tree in sync or overwrite it from a remote listing.

This explains why adopting a nonempty root requires an inventory, but repeatedly enumerating it is not sufficient. Keep a persistent mapping of local file IDs / remote identities and versions. Merge local and remote inventories, preserve unknown local data, handle skipped old placeholders explicitly, then enable on-demand population on the directories that need it. A reconnect needs reconciliation of offline changes, not reconstruction of every placeholder.

One separate diagnostic: on this host, roots under `%TEMP%` registered natively, but WinRT `GetSyncRootInformationForId` returned `ERROR_NOT_FOUND` with an inaccessible-directory message. Roots under the project's `target` directory did not exhibit this. `is_registered()` currently maps that WinRT error to `false`; inspect the original lookup error and native root state before treating it as proof that registration is absent. The exact cause of the path-dependent WinRT result remains unverified.

The callback probe uses a separate client process and exercises all 13 native callbacks:

| Rust callback | Trigger / prerequisite |
| --- | --- |
| `fetch_data` | Read missing ranges or explicitly hydrate; cached data needs no fetch. |
| `validate_data` | `ValidationRequired`: acknowledge downloaded data before completing I/O. |
| `fetch_placeholders` | Query/open beneath a partially populated directory; not ordinary or completed directories. |
| `cancel_fetch_data`, `cancel_fetch_placeholders` | Pending work becomes unnecessary; both were observed on the native timeout. |
| `opened`, `closed` | Placeholder file data/delete handles; not directory or attribute-only opens. |
| `dehydrate`, `dehydrated` | Dehydration approval and successful completion. |
| `delete`, `deleted` | Deletion approval and successful completion; inspect undelete/directory flags. |
| `rename`, `renamed` | Rename/move approval and successful completion; inspect source/target scope. |

These roles are specified in [CF_CALLBACK_TYPE](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_callback_type). In the probe, provider-issued enumeration/dehydration/rename/delete bypassed their respective callbacks, while external operations triggered them. Explicit provider hydration still triggered fetch and validation. Completed directory population suppressed repeated fetches until `UpdateOptions::has_children()` enabled on-demand population again. It is not a remote change subscription.

Both cancellation callbacks arrived after about 60 seconds with `timeout=true` and no process information; use `Request::try_process()`. Killing the reader did not reliably produce an immediate cancellation in an earlier probe. The slow timeout test is opt-in. Windows defines the [timeout and user-cancel flags](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ne-cfapi-cf_callback_cancel_flags); callback counts/order should not be treated as a transaction log. Storage Sense, Recycle Bin/undelete, cross-root moves and Windows 11 have not been tested here.

The bridge registers every callback, so a default `NotSupported` handler rejects the operation instead of omitting the callback. An approval ticket permits the local operation; it does not make a remote mutation atomic with that operation. `state_changed` is a separate directory watcher for pin/attribute changes. Lost notifications trigger a batched root scan; disconnected periods still need provider-side reconciliation.

```powershell
cargo test --test behavior --all-features -- registration_lifecycle --nocapture --test-threads=1
cargo test --test behavior --all-features -- callback_triggers --nocapture --test-threads=1
# About one minute; intentionally excluded from the default test run:
cargo test --test behavior --all-features -- callback_cancellation_timeout --ignored --nocapture --test-threads=1
```
