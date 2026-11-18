$ErrorActionPreference = 'Stop'
$path = Join-Path $env:CF_PROBE_ROOT $env:CF_PATH
$value = $env:CF_VALUE

switch ($env:CF_ACTION) {
    'mkdir' { New-Item -Path $path -ItemType Directory | Out-Null }
    'create' { New-Item -Path $path -ItemType File | Out-Null }
    'list' {
        $names = @(Get-ChildItem -LiteralPath $path | Sort-Object Name | ForEach-Object { $_.Name })
        if (($names -join ',') -cne $value) { throw "Unexpected entries: $names" }
    }
    'read' {
        if ((Get-Content -LiteralPath $path -Raw) -cne $value) { throw 'Content mismatch' }
    }
    'write' { Set-Content -LiteralPath $path -Value $value -NoNewline -Encoding Ascii }
    'rename' { Rename-Item -LiteralPath $path -NewName $value }
    'move' { Move-Item -LiteralPath $path -Destination (Join-Path $env:CF_PROBE_ROOT $value) }
    'copy' { Copy-Item -LiteralPath $path -Destination (Join-Path $env:CF_PROBE_ROOT $value) }
    'delete' { Remove-Item -LiteralPath $path -Recurse -Force }
    default { throw "Unknown test action: $env:CF_ACTION" }
}
