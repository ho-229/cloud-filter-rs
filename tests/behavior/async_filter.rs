use cloud_filter::{
    error::{CResult, CloudErrorKind},
    filter::{info, ticket, Filter, Request},
    metadata::Metadata,
    placeholder_create_info::PlaceholderCreateInfo,
    root::Session,
    utility::WriteAt,
};
use libtest_mimic::Failed;

use super::common::{Client, Root};

const CONTENT: &[u8] = b"async contents";

struct MemFilter;

impl Filter for MemFilter {
    async fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        if info.required_file_range() != (0..CONTENT.len() as u64) {
            return Err(CloudErrorKind::InvalidRequest);
        }
        ticket
            .write_at(request.file_blob(), 0)
            .map_err(|_| CloudErrorKind::Unsuccessful)
    }

    async fn fetch_placeholders(
        &self,
        _: Request,
        ticket: ticket::FetchPlaceholders,
        _: info::FetchPlaceholders,
    ) -> CResult<()> {
        for name in ["first.txt", "second.txt"] {
            ticket
                .pass_with_placeholders(&mut [PlaceholderCreateInfo::new(name)
                    .metadata(Metadata::file().size(CONTENT.len() as u64))
                    .blob(CONTENT.to_vec())
                    .mark_in_sync()])
                .map_err(|_| CloudErrorKind::Unsuccessful)?;
        }
        Ok(())
    }
}

pub fn test() -> Result<(), Failed> {
    let root = Root::new()?;
    root.register(false)?;
    let _connection = Session::new().connect_async(&root.path, MemFilter, |future| {
        futures::executor::block_on(future)
    })?;
    // An external read exercises population and hydration through AsyncBridge.
    Client::run(
        &root,
        r#"
$ErrorActionPreference = 'Stop'
$entries = @(Get-ChildItem -LiteralPath $env:CF_PROBE_ROOT)
if (($entries.Name | Sort-Object) -join ',' -cne 'first.txt,second.txt') { throw 'Unexpected population' }
foreach ($entry in $entries) {
    if ([IO.File]::ReadAllText($entry.FullName) -cne 'async contents') { throw 'Unexpected hydrated contents' }
}
"#,
    )?;
    Ok(())
}
