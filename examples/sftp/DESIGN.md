# SFTP provider design

This example uses the same Cloud Files lifecycle as Cloud Mirror, with an SFTP
directory as the backing store. Its local initialization, readonly policy,
completion-driven deletion/rename, pin handling, and task ordering follow the
design described in `../cloud-mirror/DESIGN.md`. The implementation stays separate
so the filesystem and SFTP versions can be read without a shared backend framework.

## Bind a root to a remote namespace

The SSH account's canonical home directory is the remote root. Registration stores
the endpoint, username, and that directory as the source identity; it does not
store the password. On restart, a different source is rejected rather than
silently pairing existing placeholders and local edits with another account's
files. A legacy registration with no source identity is bound when it is next
opened, while preserving its local contents and dirty state.

`remote_path` translates a client-relative path beneath this remote root and
rejects parent/root components. Population accepts only ordinary remote files and
directories, checks that returned entries belong to the requested directory, and
skips existing local names. Upload destination checks use `lstat` so a remote
symlink is not accepted as an ordinary destination. These checks are not a sandbox
against a concurrently hostile server; the remote account is assumed trusted.

`with_path` additionally handles a pending rename. The map is
`new client prefix -> old client prefix`, then `remote_path` converts the result
to an SFTP path. This ordering matters for directory descendants: a download of
`new/child.bin` must still open `old/child.bin` until the remote rename succeeds.
The map lock covers acquiring the remote handle or listing, not the entire data
transfer. This prevents a rename between path lookup and opening the remote file.

## An incremental upload still transfers the base

SFTP writes arbitrary byte ranges, but patching the published file in place would
expose a partially updated version after a failed upload. The example instead
constructs a temporary sibling and publishes it with rename:

1. Open the local file while denying writers and deletion, then record its USN.
2. If the placeholder identity matches the destination, read the server base and
   relay it into staging. Otherwise upload the complete local file.
3. Patch modified local ranges, set the final size, and include any extended tail.
4. Flush when supported, close staging, verify the local USN, and rename staging
   over the destination. Only then mark the local placeholder in sync.

Unlike a local filesystem copy, this implementation relays the base through the
client. The SSH library path used here has no server-side copy operation. The
logged count of uploaded client bytes therefore excludes potentially much larger
base-download and base-upload traffic. Modified ranges reduce local data reads;
they do not guarantee a proportional reduction in network traffic.

The `fsync` extension is optional: an unsupported-extension response is accepted,
while other failures leave the local version dirty. Replacement-by-rename depends
on server support. If it fails, the example does not unlink the original as a
fallback. Staging cleanup only removes the temporary name that this upload owns;
the reserved staging prefix also keeps unfinished uploads out of population.

## Local completion and remote completion can diverge

ACK allows Windows to attempt a local operation; it is not an SFTP transaction.
The completed local operation submits a concrete worker command. Network or
permission failures keep that command queued, and a pending rename keeps its old
path mapping available for downloads. File deletion waits for a deleting close;
directory deletion additionally verifies the original local file ID, allowing a
delete disposition to be undone without losing remote data.

Recursive deletion treats an already-missing remote entry as success. This is
necessary when retrying a partially completed tree deletion or receiving a child
notification after the parent was removed. Other errors remain failures. Moving
out of the sync root retains the remote copy; moving into it through a rename
notification is not supported.

The SFTP session is shared by the callbacks and worker, and remote operations are
blocking. Slow requests can delay other activity; a failed queued name change
also delays later name changes. There is no automatic session reconnect, durable
retry journal, remote change feed, or independent-server version reconciliation.
Restarting establishes a new session and discovers local uploads, but loses
pending delete/rename commands and their mappings.

## What the behavior tests establish

The shared cases run against an isolated, loopback-only SFTPGo process and a fresh
sync-root registration. Client file operations run in another process to trigger
real CF callbacks. Tests cover writes while handles remain open, failed remote
publication, undoing deletion, delayed completion notifications, and downloads
through both file and directory rename mappings while the remote rename fails.

This establishes the interaction with the tested server and Windows build. It
does not establish rename-replacement support, crash durability, or extension
support for every SFTP server.
