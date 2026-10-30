use std::path::Path;

use widestring::U16String;
use windows::{
    core::Result,
    Foundation::Collections::IIterable,
    Storage::{
        Provider::{StorageProviderItemProperties, StorageProviderItemProperty},
        StorageFile, StorageFolder,
    },
};

use crate::utility::ToHString;

/// A custom state of an item in a sync root, one its registration defines (see
/// [crate::root::SyncRootInfo::add_item_property_definition]): Explorer shows its icon in the
/// Status column, and its value as the icon's tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemProperty {
    /// The id of the property's definition.
    pub id: i32,
    /// What it says, as the icon's tooltip.
    pub value: String,
    /// Its icon, as an icon resource such as `%SystemRoot%\\system32\\shell32.dll,43`.
    pub icon: String,
}

/// Sets the custom states of the file or directory at `path`, in a registered sync root, to
/// `properties`, replacing those it had; none clears them. Blocks until Windows has them.
pub fn set_item_properties(path: impl AsRef<Path>, properties: &[ItemProperty]) -> Result<()> {
    let path = path.as_ref();
    let name = U16String::from_os_str(path.as_os_str()).to_hstring();
    let mut list = Vec::with_capacity(properties.len());
    for p in properties {
        let property = StorageProviderItemProperty::new()?;
        property.SetId(p.id)?;
        property.SetValue(&U16String::from_str(&p.value).to_hstring())?;
        property.SetIconResource(&U16String::from_str(&p.icon).to_hstring())?;
        list.push(Some(property));
    }
    let list = IIterable::<StorageProviderItemProperty>::try_from(list)?;
    if path.is_dir() {
        let folder = StorageFolder::GetFolderFromPathAsync(&name)?.get()?;
        StorageProviderItemProperties::SetAsync(&folder, &list)?.get()
    } else {
        let file = StorageFile::GetFileFromPathAsync(&name)?.get()?;
        StorageProviderItemProperties::SetAsync(&file, &list)?.get()
    }
}
