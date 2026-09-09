param([string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
# Never build over the executable used by a live checkout supervisor.
$build = Join-Path $Root 'target\portal-package'
if (-not $SkipBuild) {
    Push-Location -LiteralPath $Root
    try {
    & cargo build --release --locked --manifest-path (Join-Path $Root 'Cargo.toml') --target-dir $build
    if ($LASTEXITCODE -ne 0) { throw 'Windows release build failed.' }
    } finally { Pop-Location }
}
$exe = Join-Path $build 'release\heart-portal.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "Missing release binary: $exe" }
$manifest = [IO.File]::ReadAllText((Join-Path $Root 'portal\Cargo.toml'))
$sourceVersion = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $sourceVersion -or (& $exe --version).Trim() -ne "heart-portal $sourceVersion") {
    throw 'Build cache version differs from source. Rebuild before packaging; do not ship an E2E fixture.'
}
$dist = Join-Path $Root 'dist'
$package = Join-Path $dist 'heart-portal-windows-x86_64'
[IO.Directory]::CreateDirectory((Join-Path $package 'scripts')) | Out-Null
Copy-Item -LiteralPath $exe -Destination (Join-Path $package 'heart-portal.exe') -Force
Copy-Item -LiteralPath $exe -Destination (Join-Path $dist 'heart-portal-windows-x86_64.exe') -Force
Copy-Item -LiteralPath (Join-Path $Root 'portal.example.toml') -Destination $package -Force
foreach ($name in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-bootstrap.ps1', 'portal-supervisor-hidden.vbs', 'portal-task-common.ps1', 'install-portal-task.ps1', 'install-portal-windows.ps1', 'uninstall-portal-task.ps1')) {
    Copy-Item -LiteralPath (Join-Path $Root "scripts\$name") -Destination (Join-Path $package 'scripts') -Force
}
Compress-Archive -LiteralPath $package -DestinationPath (Join-Path $dist 'heart-portal-windows-x86_64.zip') -Force
Get-FileHash -LiteralPath (Join-Path $dist 'heart-portal-windows-x86_64.exe') -Algorithm SHA256
