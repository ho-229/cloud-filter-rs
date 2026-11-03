# CFAPI behavior notes

These observations come from Windows 10 IoT Enterprise LTSC 2021, build **19044.7807**,
on NTFS. They explain provider design constraints and library workarounds; other Windows
builds may behave differently. Relevant probes are in [lifecycle.rs](lifecycle.rs) and
[tickets.rs](tickets.rs).

Registration persists across provider restarts. Normal startup and shutdown should use
connect/disconnect. Updating a registration preserved child identities and data, but reset
the root to partially populated on this host. Re-register only when settings change.

[Unregister](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfunregistersyncroot)
changes the local tree: fully hydrated files and fully populated directories become ordinary
entries; other placeholders are deleted. It can skip entries it cannot process. A locked
placeholder survived successful unregister with its identity intact, while a partially
hydrated file was deleted. Registering again therefore left a mixed tree of ordinary files
and old placeholders. Unregister belongs to removing a sync relationship, after disconnecting.

Registering a nonempty directory leaves ordinary children unchanged. They gain neither
identity nor sync state, and ordinary subdirectories do not request remote children until
converted with on-demand population enabled. Adopting such a root requires comparing local
and remote contents and resolving conflicts before conversion. Recreating existing names
with `PlaceholderCreateInfo` fails; blindly marking survivors in sync can hide local changes.

Approval callbacks and completion notifications do not form a durable transaction log.
Setting and then clearing delete disposition produced `deleted` followed by an undelete
request, yet the file survived and `closed.deleted()` was false. A rename collision introduced
during approval let ACK succeed while the local rename failed and no `renamed` arrived.
Remote mutations therefore need to account for the final local outcome, including undo.

Provider-issued enumeration, dehydration, rename and delete bypassed their corresponding
callbacks, while external operations triggered them. Explicit provider hydration still
triggered fetch and validation. Test user actions from another process. `opened`/`closed`
covered file data/delete handles, but did not cover directories or attribute-only opens;
they cannot settle every kind of namespace change.

Completed directory population suppressed later fetches until
`UpdateOptions::has_children()` enabled on-demand population again. Discovering subsequent
remote changes remains the provider's responsibility.

Cancellation can arrive late and without process information. Killing the reader did not
reliably produce an immediate callback; both fetch cancellation callbacks were observed on
timeout, after about 60 seconds.

Hydration restart worked with FULL policy while preserving file size. A restart that changed
size stalled inside `CfExecute`; the library consequently rejects size changes with
`ERROR_NOT_SUPPORTED`. This is a workaround for the observed stall, not a documented CFAPI
restriction. With PARTIAL policy, restart returned `ERROR_CLOUD_FILE_INVALID_REQUEST` rather
than the `STATUS_CLOUD_FILE_NOT_SUPPORTED` specified in the
[native documentation](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/ns-cfapi-cf_operation_parameters).
A rejected restart left the original read pending: the provider still had to service or fail it.

WinRT registration lookup and native root state can disagree. Roots under `%TEMP%` registered
natively, but WinRT lookup returned `ERROR_NOT_FOUND` with an inaccessible-directory message;
roots under the project's `target` directory did not exhibit this. The cause remains
unverified. Since `SyncRootId::is_registered()` maps that error to `false`, inspect the lookup
error and native state before deciding that registration is absent.

Handle-based root-info snapshots can also lag provider status updates. Just after connecting,
a snapshot still reported `Disconnected` while `CfQuerySyncProviderStatus` returned the newly
set status; later handle queries caught up.
