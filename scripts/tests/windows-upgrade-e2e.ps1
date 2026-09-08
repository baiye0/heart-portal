# Build a higher-version real exe in an isolated source copy. The published
# package keeps its actual version; no GitHub release or production task changes.
param([string]$Binary = (Join-Path $PSScriptRoot '..\..\dist\heart-portal-windows-x86_64.exe'))
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$version = (& $binaryPath --version).Trim().Split(' ')[1]
$parts = $version.Split('.')
$nextVersion = '{0}.{1}.{2}' -f $parts[0], $parts[1], ([int]$parts[2] + 1)
$fixture = Join-Path $repo ('target\upgrade-e2e-source-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory((Join-Path $fixture 'portal')) | Out-Null
[IO.Directory]::CreateDirectory((Join-Path $fixture 'scripts')) | Out-Null
foreach ($name in @('Cargo.toml','Cargo.lock','portal.example.toml')) { Copy-Item -LiteralPath (Join-Path $repo $name) -Destination $fixture }
Copy-Item -LiteralPath (Join-Path $repo 'portal\src') -Destination (Join-Path $fixture 'portal') -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'portal\Cargo.toml') -Destination (Join-Path $fixture 'portal')
Get-ChildItem -LiteralPath (Join-Path $repo 'scripts') -File | Where-Object { $_.Extension -in @('.ps1','.vbs') } | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $fixture 'scripts') }
$manifestPath = Join-Path $fixture 'portal\Cargo.toml'
$manifest = [IO.File]::ReadAllText($manifestPath).Replace(('version = "{0}"' -f $version), ('version = "{0}"' -f $nextVersion))
[IO.File]::WriteAllText($manifestPath, $manifest)
$lockPath = Join-Path $fixture 'Cargo.lock'
$lockText = [IO.File]::ReadAllText($lockPath)
$lockText = [regex]::Replace($lockText, '(name = "heart-portal"\r?\nversion = ")[^"]+("\r?\n)', { param($m) $m.Groups[1].Value + $nextVersion + $m.Groups[2].Value })
[IO.File]::WriteAllText($lockPath, $lockText)
# Keep test-version outputs separate from both production packaging and the
# live checkout. Each can reuse dependencies without sharing the final exe.
$build = Join-Path $repo 'target\portal-upgrade-e2e'
# Force this package to relink: distinct source copies share the final exe path,
# so a cached artifact must not leave the preceding build's version there.
Push-Location -LiteralPath $repo
try {
    & cargo rustc --release --locked -p heart-portal --bin heart-portal --manifest-path (Join-Path $fixture 'Cargo.toml') --target-dir $build -- -C "metadata=upgrade-e2e-$([guid]::NewGuid().ToString('N'))"
    if ($LASTEXITCODE -ne 0) { throw 'Higher-version fixture build failed.' }
} finally { Pop-Location }
$candidate = Join-Path $fixture 'heart-portal.exe'
Copy-Item -LiteralPath (Join-Path $build 'release\heart-portal.exe') -Destination $candidate
if ((& $candidate --version).Trim() -ne "heart-portal $nextVersion") { throw 'Fixture version does not match the intended upgrade.' }
& (Join-Path $PSScriptRoot 'windows-package.tests.ps1') -Binary $binaryPath -Candidate $candidate
Write-Output "PASS: real single-exe command-line upgrade $version -> $nextVersion"
