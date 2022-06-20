# wincs
[![Latest Release](https://img.shields.io/github/v/release/ok-nick/wincs?include_prereleases)](https://github.com/ok-nick/wincs/releases/latest)
[![Windows Cloud Sync Engine](https://img.shields.io/badge/-Windows%20Cloud%20Sync%20Engine-blueviolet)](https://github.com/ok-nick/wincs)
[![Discord](https://img.shields.io/discord/834969350061424660)](https://discord.gg/w9Bc6xH7uC)

`wincs` is a safe and idiomatic wrapper around the native [Windows Cloud Filter API](https://docs.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine). Because it is built around the Cloud Filter API, it has first-class Windows features that are only supported through its API.
For example:
* [Placeholder files](#what-are-placeholders)
    * Partial files
    * Full files
    * Pinned files
* Built into the native File Explorer
    * Shown in the navigation pane at the uppermost level
    * Group sync engines as siblings
    * Register/unregister and connect/disconnect sync engines
        * Register to the File Explorer
        * Connect your file operation handlers
    * Custom name tag and icon
    * Automatic/custom hydration state icons
    * Progress indication
        * File dialog
        * Inline next to file in the File Explorer
        * A file toast if the user did not explicitly hydrate the placeholder
    * Thumbnails and metadata
    * Top-level context menu actions (even on Windows 11)
* Block specific apps from hydrating placeholders from Windows settings
* Automatically free space via Windows Storage Sense
* Monitor and filter file operations
* Declare a wide variety of supported file properties
* Files are cached to the disk (if set), allowing for offline access
* TODO: There is also a new API for custom UI, IStorageProviderStatusUISource

As of right now, the Cloud Filter API is used in production by OneDrive, Google Drive, Dropbox, and many other clients.

## TODO
Documentation needs to be added and refined. Grep `TODO` to find a list of unsolved issues. In addition, there are many unimplemented features included with `TODO` comments. The API is subject to change and I am open to opinions for change. The [examples directory](https://github.com/ok-nick/wincs/tree/main/examples) is outdated and needs refinement, as well as commenting. CI and CD are also needed.

If anyone is interested in contributing, feel free to leave an issue or PR.

## Example
Below is a simple snippet of implementing a sync engine. For more, in-depth examples, please check out the [examples directory](https://github.com/ok-nick/wincs/tree/main/examples).
```rs
// TODO
```

## FAQ

### Why not FUSE?
Unfortunately, FUSE is only implemented on Unix-like operating systems. Luckily, there are numerous alternatives for implementing file systems on Windows, such as `dokany` or `winfsp`.

#### Why not `dokany`?
`dokany` has a Rust API and is accessible using safe code. However, it is fairly low-level and does not have the first-class capabilities supported by `wincs`. Read more [here](#wincs).

#### Why not `winfsp`?
Unlike `dokany`, `winfsp` currently does not have a Rust API. Perhaps at some point it may, but even so, it is impossible to have the first-class features supported by `wincs`. Read more [here](#wincs).

### What are placeholders?
Placeholders are another term for NTFS sparse files. To put it simple, they are empty files that are meant to represent real files, although are not backed by any allocation unless requested. The way they work is heavily dependent on the sync engines' configuration. Know that if a process were to read the content of the placeholder, it would be "hydrated" (its file contents would be allocated). For more information, read [here](https://docs.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine). 

### I know `wincs` is maintained, but does Microsoft maintain the Cloud Filter API?
Of course, it is used by Microsoft's very own OneDrive Client. I have reported numerous issues and received quick feedback via the [Microsoft Q&A](https://docs.microsoft.com/en-us/answers/search.html?c=7&includeChildren=false&type=question&redirect=search%2Fsearch&sort=newest&q=cfapi). There are a lot of undocumented and unimplemented portions of the API, although they are not necessary for the features described [here](#wincs).

## Additional Resources
If you are looking to contribute or want a deeper understanding of `wincs`, be sure to check out these resources:

