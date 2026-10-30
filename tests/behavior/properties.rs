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
        let path =
            std::env::temp_dir().join(format!("cf-properties-{}-{stamp}", std::process::id()));
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
    let script = format!(
        "$i = (New-Object -ComObject Shell.Application).Namespace('{}').ParseName('{}'); \
         $v = $i.ExtendedProperty('System.StorageProviderCustomStates'); \
         if ($v) {{ $b = [byte[]]$v; [Text.Encoding]::Unicode.GetString($b) + '|' + \
         [Text.Encoding]::Unicode.GetString($b, 1, $b.Length - 1) }}",
        path.parent().unwrap().display(),
        path.file_name().unwrap().to_string_lossy()
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .context("run powershell")?;
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
    let directory = TestDirectory::new()?;
    let id = SyncRootIdBuilder::new(directory.0.file_name().unwrap())
        .user_security_id(SecurityId::current_user()?)
        .build();
    id.register(
        SyncRootInfo::default()
            .with_display_name("Properties Behavior Test")
            .with_icon("%SystemRoot%\\system32\\charmap.exe,0")
            .with_version("1.0.0")
            .with_hydration_type(HydrationType::Full)
            .with_population_type(PopulationType::AlwaysFull)
            .with_item_property_definition(STATE, STATE_NAME)?
            .with_path(&directory.0)?,
    )
    .context("register temporary root")?;
    let registration = Registration(id);

    assert_eq!(
        registration.0.info()?.item_property_definitions(),
        vec![(STATE, OsString::from(STATE_NAME))],
        "the registration keeps its definitions"
    );

    let _connection = Session::new().connect(&directory.0, Filter)?;
    PlaceholderCreateInfo::new("marked.txt")
        .metadata(Metadata::file().size(0))
        .blob(b"file".to_vec())
        .mark_in_sync()
        .create(&directory.0)
        .context("create file placeholder")?;
    PlaceholderCreateInfo::new("marked")
        .metadata(Metadata::directory())
        .blob(b"folder".to_vec())
        .has_no_children()
        .mark_in_sync()
        .create(&directory.0)
        .context("create folder placeholder")?;
    let (file, folder) = (directory.0.join("marked.txt"), directory.0.join("marked"));

    set_item_properties(&file, &[state("on the file")]).context("set on a file")?;
    set_item_properties(&folder, &[state("on the folder")]).context("set on a folder")?;
    let states = custom_states(&file)?;
    assert!(states.contains("on the file"), "{states:?}");
    let states = custom_states(&folder)?;
    assert!(states.contains("on the folder"), "{states:?}");

    // None clears them.
    set_item_properties(&file, &[]).context("clear")?;
    let states = custom_states(&file)?;
    assert!(!states.contains("on the file"), "{states:?}");
    Ok(())
}
