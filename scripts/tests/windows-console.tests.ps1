# Exercise the actual reader with split writes, redaction, and log truncation.
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$tempBase = [IO.Path]::GetTempPath()
$root = Join-Path $tempBase ("portal console test ' " + [char]0x6D4B + '-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($root)
$reader = $null
$script:pendingLine = $null
$script:observed = ''
function Wait-Line([string]$Expected) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt 10) {
        if (-not $script:pendingLine) { $script:pendingLine = $reader.StandardOutput.ReadLineAsync() }
        if ($script:pendingLine.Wait(200)) {
            $line = $script:pendingLine.Result
            $script:pendingLine = $null
            if ($null -eq $line) { throw "Reader exited: $($reader.StandardError.ReadToEnd())" }
            $script:observed += "$line`n"
            if ($line.Contains($Expected)) { return }
        }
    }
    throw "Reader did not show '$Expected'. Output: $script:observed"
}
try {
    [void][IO.Directory]::CreateDirectory((Join-Path $root 'scripts'))
    Copy-Item -LiteralPath (Join-Path $repo 'scripts\portal-lifecycle.ps1') -Destination (Join-Path $root 'scripts')
    $utf8 = [Text.UTF8Encoding]::new($false)
    $log = Join-Path $root 'portal-runtime.log'
    [IO.File]::WriteAllText($log, "startup-history`nPortal relay handshake OK - historical log text`n", $utf8)
    [IO.File]::WriteAllText((Join-Path $root 'portal-runtime.err.log'), '', $utf8)
    $runtime = @{ pid=$PID; started=(Get-Process -Id $PID).StartTime.ToUniversalTime().Ticks; nonce='one' }
    Write-PortalJson (Join-Path $root '.portal-runtime.json') $runtime
    Write-PortalJson (Join-Path $root '.portal-connection-status.json') @{ pid=$PID; nonce='stale'; state='connected' }
    Write-PortalJson (Join-Path $root '.portal-launch.json') @{ environment=@{ PORTAL_CONNECT_LINK='https://example.invalid/being/?token=fixture-secret-token' } }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $info.Arguments = '-NoProfile -NonInteractive -Command ' + (ConvertTo-PortalArgument ([IO.File]::ReadAllText((Join-Path $repo 'scripts\portal-console.ps1'))))
    $info.EnvironmentVariables['HEART_PORTAL_CONSOLE_ROOT'] = $root
    $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    $info.StandardOutputEncoding=$utf8; $info.StandardErrorEncoding=$utf8
    $reader = [Diagnostics.Process]::Start($info)
    Wait-Line 'startup-history'
    [IO.File]::AppendAllText($log, 'token=fixture-secret-', $utf8)
    Start-Sleep -Milliseconds 450
    [IO.File]::AppendAllText($log, "token`n", $utf8)
    Wait-Line 'token=<redacted>'
    if ($script:observed.Contains('fixture-secret-token')) { throw 'Reader exposed a token.' }
    # Split one UTF-8 character between writes, as a pipe or process may do.
    $bytes = $utf8.GetBytes("中文日志`n")
    $file = [IO.File]::Open($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
    try {
        $file.Write($bytes,0,1); $file.Flush()
        Start-Sleep -Milliseconds 450
        $file.Write($bytes,1,$bytes.Length-1); $file.Flush()
    } finally { $file.Dispose() }
    Wait-Line '中文日志'
    if ($script:observed.Contains('Being 连接状态：已连接 Being')) { throw 'Stale state or old log text incorrectly claimed a connection.' }
    Write-PortalJson (Join-Path $root '.portal-connection-status.json') @{ pid=$PID; nonce='one'; state='connecting' }
    Wait-Line 'Being 连接状态：正在连接 Being'
    Write-PortalJson (Join-Path $root '.portal-connection-status.json') @{ pid=$PID; nonce='one'; state='connected' }
    Wait-Line 'Being 连接状态：已连接 Being'
    [IO.File]::WriteAllText($log, "restarted-runtime`n", $utf8)
    $runtime.nonce = 'two'
    Write-PortalJson (Join-Path $root '.portal-runtime.json') $runtime
    Wait-Line 'restarted-runtime'
    Wait-Line '等待当前进程的连接状态'
    Write-Output 'PASS: console streams/redacts UTF-8 logs, follows restarts and displays connection state only for the current PID/nonce'
} finally {
    if ($reader) { if (-not $reader.HasExited) { $reader.Kill(); [void]$reader.WaitForExit(5000) }; $reader.Dispose() }
    $resolved = [IO.Path]::GetFullPath($root)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -notlike 'portal console test *') { throw 'Unsafe console fixture cleanup path.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
