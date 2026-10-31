use std::path::Path;

use windows::{
    core::{Result, HSTRING},
    Foundation::Collections::IIterable,
    Storage::{
        Provider::{StorageProviderItemProperties, StorageProviderItemProperty},
        StorageFile, StorageFolder,
    },
};

use crate::utility::storage_path;

/// A custom state of an item in a sync root, one its registration defines (see
/// [crate::root::SyncRootInfo::add_item_property_definition]): Explorer shows its icon in the
/// Status column, and its value as the icon's tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemProperty {
    /// The id of the property's definition.
    pub id: i32,
    /// What it says, as the icon's tooltip.
    pub value: String,
    /// Its icon, as an icon resource such as `%SystemRoot%\system32\shell32.dll,43`.
    pub icon: String,
}

/// Sets the custom states of the file or directory at `path`, in a registered sync root, to
/// `properties`, replacing those it had; an empty slice clears them. Blocks until Windows has them.
/// Relative paths are resolved against the current directory.
pub fn set_item_properties(path: impl AsRef<Path>, properties: &[ItemProperty]) -> Result<()> {
    let path = storage_path(path.as_ref())?;
    let name = HSTRING::from(path.as_os_str());
    let list = properties
        .iter()
        .map(|p| {
            let property = StorageProviderItemProperty::new()?;
            property.SetId(p.id)?;
            property.SetValue(&HSTRING::from(&p.value))?;
            property.SetIconResource(&HSTRING::from(&p.icon))?;
            Ok(Some(property))
        })
        .collect::<Result<Vec<_>>>()?;
    let list = IIterable::<StorageProviderItemProperty>::try_from(list)?;
    if path.is_dir() {
        let folder = StorageFolder::GetFolderFromPathAsync(&name)?.get()?;
        StorageProviderItemProperties::SetAsync(&folder, &list)?.get()
    } else {
        let file = StorageFile::GetFileFromPathAsync(&name)?.get()?;
        StorageProviderItemProperties::SetAsync(&file, &list)?.get()
    }
}
