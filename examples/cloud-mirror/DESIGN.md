# Cloud Mirror design

Cloud Mirror uses an ordinary directory as the remote store so that the Cloud
Files lifecycle can be studied without a network protocol. The client directory
contains placeholders and local edits; the server directory contains the backing
file contents. CFAPI supplies the placeholder mechanism, but the example decides
when to upload, what to keep remotely, and when to acknowledge synchronization.

## Registration and connection have different lifetimes

Registration gives Windows the root's identity and policies. A connection attaches
the running provider's callbacks. Disconnecting therefore keeps the registration
and placeholders; unregistering is not part of normal shutdown and can remove
dehydrated placeholders.

`register_root` requires an empty directory for a new registration. For an existing
registration, it checks the client path and the stored server identity, then keeps
the existing files and dirty state. It only updates older registrations that lack
the source identity or required policies. Re-registering on every start could
reset population state, while accepting a different server could associate local
edits with unrelated remote contents.

## Population, contents, and synchronization are separate

`fetch_placeholders` describes a requested directory's immediate children. It
skips existing local entries instead of replacing local edits during enumeration.
`fetch_data` opens the corresponding server file and supplies the requested byte
range. A size mismatch fails hydration rather than transferring a truncated or
incompatible source. Metadata queries use handles without data access so that
inspecting a placeholder does not itself download its contents.

`InSync` is the provider's acknowledgment that an entry matches its backing store;
it does not mean that its contents are resident locally. Conversely, downloading
a file does not upload a local edit. Each directory's own entry is marked in sync
after its server directory exists, independently of its children. Otherwise a
directory can remain unsynchronized even after every child file has uploaded.

## Discover changes, then perform I/O

After connecting, the example sends the root through `state_changed` once to find
offline edits. Later notifications use the same path: inspect entries, traverse
directories, and submit individual paths as they are discovered. There is no
periodic full-tree scan. The library handles notification-buffer overflow by
reporting a rescan in batches; a stopped watcher requires restarting the provider.

`SyncWorker` coalesces path notifications and retries failed tasks. It checks an
entry's current state when executing it, because a queued notification is not a
snapshot. Delete and rename commands run in queue order before uploads to their
affected paths. A failed command remains at the front, so it also delays later
delete and rename commands. The queue is intentionally in memory only.

## Publish an upload before marking it in sync

The upload path has three distinct responsibilities:

- `upload` acquires a local read handle that denies writers and deletion, checks
  the version, publishes the result, and finally marks the placeholder in sync.
- `upload_destination` resolves and checks the server destination and creates
  ordinary parent directories. Reparse points must not redirect writes elsewhere.
- `copy_upload` builds a staging file. When the placeholder identity still matches
  the destination, it copies the server base and patches CF's modified ranges.
  Otherwise it copies the complete local file.

Modified ranges alone do not describe a size change: truncation explicitly sets
the staging length, and extension adds the entire new tail, including zero-filled
gaps. Missing server bases also require a full upload. A path identity is not a
version identifier; the optimization assumes the server base was not independently
edited.

The local handle remains open through publication and the final in-sync update.
The code also compares the file USN before and after copying. On the tested Windows
10 build, the USN-conditioned in-sync call failed despite a matching USN, so the
example uses the retained write-denying handle and the no-USN form instead. It
also checks modified-data size, since appends were observed to leave `InSync` set.
These are explicit workarounds, not a general conflict-resolution protocol.

## Permission callbacks are not commit notifications

| Event | Effect on the backing store |
| --- | --- |
| `delete`, `rename` | Check permission and ACK; do not change remote names. |
| `closed(deleted=true)` | Queue remote file deletion. |
| Directory `deleted` | Queue an ID check; delete remotely only after the original local object is gone. |
| `renamed` | Queue the remote rename. |
| `undelete` | Preserve remote contents. |

The distinction matters because a successful delete disposition can be undone
before close, and a rename can fail locally after ACK. Directories have no close
callback, so their delayed task opens the original file ID without delete sharing:
an outstanding deleting handle means wait, an existing object means retain the
server copy, and a missing ID permits deletion. A missing pathname alone would
also describe a rename or replacement and is insufficient.

A local rename can finish while the server still refuses its rename. During this
interval, `renames` maps the new client prefix to the old client prefix.
`with_path` applies that map, including directory descendants, before opening the
backing file or listing a directory. It holds the map lock through that open so a
concurrent remote rename cannot invalidate the resolved name. Successful remote
rename removes the mapping. Overlapping rename/delete requests are rejected while
the mapping is pending.

## Residency and limits

Pin changes are intent: the provider hydrates pinned files and considers unpinned
files for dehydration. It only releases resident data when the file is in sync,
has no modified ranges, and the server size still matches. Provider-issued
dehydration can bypass the permission callback, so this check also lives at the
call site. Readonly mode preserves local edits without uploading or marking them
in sync; it still permits downloads and residency management.

There is no durable task journal, remote change feed, or version conflict model.
Pending tasks are lost on disconnect; startup finds local contents to upload but
cannot reconstruct offline deletions or renames. Size checks cannot detect an
independent same-size server edit. The behavior cases in
`../common/behavior.rs` exercise these mechanisms through another process, since
provider-originated file operations can bypass CF callbacks.
