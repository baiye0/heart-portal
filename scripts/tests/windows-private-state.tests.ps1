# Native Windows PowerShell 5.1 regression: broad directories must not expose credentials.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\portal-lifecycle.ps1')
$root = Join-Path ([IO.Path]::GetTempPath()) ('portal ACL ' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($root) | Out-Null
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
$everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')

function Assert-Private([string]$Path) {
    $acl = [IO.File]::GetAccessControl($Path)
    if (-not $acl.AreAccessRulesProtected) { throw "Inherited ACL on $Path" }
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    if (-not ($rules | Where-Object { $_.IdentityReference -eq $user -and $_.AccessControlType -eq 'Allow' })) {
        throw "Owner cannot access $Path"
    }
    foreach ($rule in $rules) {
        if ($rule.IdentityReference.Value -notin @($user.Value, 'S-1-5-18')) { throw "Unrelated principal on $Path" }
    }
}

try {
    $acl = [IO.Directory]::GetAccessControl($root)
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone,
        [Security.AccessControl.FileSystemRights]::ReadAndExecute,
        [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit',
        [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Allow))
    [IO.Directory]::SetAccessControl($root, $acl)
    # Direct snapshot and journal contain copies of the same credential. Include
    # start requests and saved launch state, not only the originally reported file.
    foreach ($name in @('request.json', '.portal-launch.json', '.portal-direct.json', '.portal-upgrade.json')) {
        $path = Join-Path $root $name
        Write-PortalJson $path @{ environment = @{ PORTAL_MCP_TOKEN = 'private-test-token'; PORTAL_CONNECT_LINK = 'https://example.invalid/a/?token=fixture' } }
        Assert-Private $path
        # File.Replace would preserve this explicitly broad destination ACL.
        $broad = [IO.File]::GetAccessControl($path)
        $broad.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone, 'Read', 'Allow'))
        [IO.File]::SetAccessControl($path, $broad)
        Write-PortalJson $path @{ environment = @{ PORTAL_MCP_TOKEN = 'rotated-test-token' } }
        Assert-Private $path
        if ((Read-PortalJson $path).environment.PORTAL_MCP_TOKEN -ne 'rotated-test-token') { throw 'Credential changed during protected replacement' }
        $bytes = [IO.File]::ReadAllBytes($path)
        [IO.File]::SetAccessControl($path, $broad)
        Read-PortalJson $path | Out-Null
        Assert-Private $path
        if ([Convert]::ToBase64String($bytes) -ne [Convert]::ToBase64String([IO.File]::ReadAllBytes($path))) { throw 'ACL migration changed file bytes' }
    }
    $link = Join-Path $root '.portal-connection.url'
    Write-PortalPrivateText $link 'https://example.invalid/a/?token=legacy'
    Assert-Private $link
    if ([IO.File]::ReadAllText($link) -ne 'https://example.invalid/a/?token=legacy') { throw 'Legacy connection changed' }
    # Observe the still-open temp file at the actual write boundary, before
    # publication. There must never be a broadly readable secret temp file.
    $security = New-PortalFileSecurity
    $temp = Join-Path $root 'unpublished.tmp'
    $stream = [IO.FileStream]::new($temp, [IO.FileMode]::CreateNew,
        [Security.AccessControl.FileSystemRights]::FullControl, [IO.FileShare]::Read,
        4096, [IO.FileOptions]::None, $security)
    try { Assert-Private $temp } finally { $stream.Dispose() }
    if (Get-ChildItem -LiteralPath $root -Filter '*.json.*.tmp') { throw 'Publication left temporary files' }
    Write-Output 'PASS: private creation, replacement, legacy repair, all credential copies, and unchanged values'
} finally { Remove-Item -LiteralPath $root -Recurse -Force }
