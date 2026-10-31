use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Request, SyncFilter},
    metadata::Metadata,
    placeholder::{ConvertOptions, Placeholder},
    placeholder_create_info::PlaceholderCreateInfo,
    properties::{set_item_properties, ItemProperty},
    root::{
        HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
        SyncRootInfo,
    },
};
use libtest_mimic::Failed;

const STATE: i32 = 1;
const STATE_NAME: &str = "Behavior test state";

// Only clean up a unique directory successfully created by this test.
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> std::io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // In the user's profile, as sync roots are: the shell cannot reach the states of an item
        // under a hidden folder, such as AppData, where the temporary directory is.
        let profile =
            std::env::var_os("USERPROFILE").map_or_else(std::env::temp_dir, PathBuf::from);
        let path = profile.join(format!("cf-properties-{}-{stamp}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Registration(SyncRootId);

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.0.unregister();
    }
}

struct Filter;

impl SyncFilter for Filter {
    fn fetch_data(&self, _: Request, _: ticket::FetchData, _: info::FetchData) -> CResult<()> {
        Err(CloudErrorKind::NotSupported)
    }
}

/// The custom states Explorer has for `path`, as text, read back through the shell as Explorer
/// reads them. The property is a serialized blob in which a string may start at an odd offset,
/// so it is decoded at both.
fn custom_states(path: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", r#"
            $ErrorActionPreference = 'Stop'
            [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
            $i = (New-Object -ComObject Shell.Application).Namespace($env:CF_PROPERTY_PARENT).ParseName($env:CF_PROPERTY_NAME)
            if ($null -eq $i) { throw 'Shell item not found' }
            $v = $i.ExtendedProperty('System.StorageProviderCustomStates')
            if ($v) {
                $b = [byte[]]$v
                [Text.Encoding]::Unicode.GetString($b) + '|' + [Text.Encoding]::Unicode.GetString($b, 1, $b.Length - 1)
            }
        "#])
        .env("CF_PROPERTY_PARENT", path.parent().unwrap())
        .env("CF_PROPERTY_NAME", path.file_name().unwrap())
        .output()
        .context("run powershell")?;
    anyhow::ensure!(
        output.status.success(),
        "read custom states: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).replace('\0', ""))
}

fn state(value: &str) -> ItemProperty {
    ItemProperty {
        id: STATE,
        value: value.to_owned(),
        icon: "%SystemRoot%\\system32\\shell32.dll,43".to_owned(),
    }
}

pub fn custom_states_show_in_explorer() -> Result<(), Failed> {
    let directory = TestDirectory::new().context("create the root's directory")?;
    let id = SyncRootIdBuilder::new(directory.0.file_name().unwrap())
        .user_security_id(SecurityId::current_user().context("current user")?)
        .build();
    id.register(
        SyncRootInfo::default()
            .with_display_name("Properties Behavior Test")
            .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
            .with_version("1.0.0")
            .with_hydration_type(HydrationType::Full)
            .with_population_type(PopulationType::AlwaysFull)
            .with_item_property_definition(STATE, STATE_NAME)
            .context("add the definition")?
            .with_path(fs::canonicalize(&directory.0)?)
            .context("set the root's path")?,
    )
    .context("register temporary root")?;
    let registration = Registration(id);
    assert_eq!(
        SyncRootId::from_path(directory.0.to_string_lossy().replace('\\', "/"))?.as_hstring(),
        registration.0.as_hstring(),
        "look up the registered root through a normalized path"
    );

    assert_eq!(
        registration
            .0
            .info()
            .context("read the registration")?
            .item_property_definitions(),
        vec![(STATE, OsString::from(STATE_NAME))],
        "the registration keeps its definitions"
    );

    let _connection = Session::new()
        .connect(&directory.0, Filter)
        .context("connect")?;
    PlaceholderCreateInfo::new("marked'文件.txt")
        .metadata(Metadata::file().size(8192))
        .blob(b"file".to_vec())
        .mark_in_sync()
        .create(&directory.0)
        .context("create file placeholder")?;
    // Creating a folder placeholder that is in sync is refused; converting a plain folder works.
    let (file, folder) = (
        directory.0.join("marked'文件.txt"),
        directory.0.join("marked"),
    );
    fs::create_dir(&folder).context("create folder")?;
    Placeholder::options()
        .write_access()
        .exclusive()
        .open(&folder)
        .context("open folder")?
        .convert_to_placeholder(ConvertOptions::default().mark_in_sync(), None)
        .context("convert folder")?;

    for (path, value) in [
        (file.clone(), "absolute path"),
        (
            PathBuf::from(file.to_string_lossy().replace('\\', "/")),
            "forward slashes",
        ),
        (fs::canonicalize(&file)?, "扩展路径"),
    ] {
        set_item_properties(&path, &[state(value)]).context("set on a file")?;
        let states = custom_states(&file)?;
        assert!(states.contains(value), "{states:?}");
    }
    let info = Placeholder::open(&file)?.info()?.unwrap();
    assert_eq!(
        info.on_disk_data_size(),
        0,
        "setting properties must not hydrate"
    );
    assert!(info.is_in_sync());
    set_item_properties(&folder, &[state("on the folder")]).context("set on a folder")?;
    let states = custom_states(&folder)?;
    assert!(states.contains("on the folder"), "{states:?}");

    assert!(custom_states(&directory.0.join("missing.txt")).is_err());

    // An empty list clears them.
    set_item_properties(&file, &[]).context("clear")?;
    let states = custom_states(&file)?;
    assert!(!states.contains("扩展路径"), "{states:?}");
    Ok(())
}
