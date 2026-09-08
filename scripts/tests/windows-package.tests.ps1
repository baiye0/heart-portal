# Start with ONLY the shipped exe; create/remove this fixture's own logon task.
param([string]$Binary = (Join-Path $PSScriptRoot '..\..\dist\heart-portal-windows-x86_64.exe'), [string]$Candidate = '', [switch]$LocalOnly)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$tempBase = [IO.Path]::GetTempPath()
$id = [guid]::NewGuid().ToString('N')
$testRoot = Join-Path $tempBase ("portal single exe test ' " + [char]0x6D4B + '-' + $id)
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
$exe = Join-Path $testRoot 'heart-portal-windows-x86_64.exe'
Copy-Item -LiteralPath $Binary -Destination $exe
$worker = $null
$testFailure = $null
function Assert([bool]$Value, [string]$Message) { if (-not $Value) { throw "Assertion failed: $Message" } }
function Wait-Until([scriptblock]$Condition, [string]$Message, [int]$Timeout = 90) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $Timeout) {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 200
    }
    throw "Timed out: $Message"
}
function Start-PortalCli([string[]]$Arguments = @(), [string]$Directory = $testRoot) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $exe
    $info.Arguments = (@($Arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
    $info.WorkingDirectory = $Directory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.EnvironmentVariables['HOME'] = Join-Path $testRoot 'test-home'
    $info.EnvironmentVariables['USERPROFILE'] = Join-Path $testRoot 'test-home'
    $info.EnvironmentVariables['PORTAL_CONNECT_LINK'] = if ($LocalOnly) { '' } else { "https://relay.invalid/clean-$id/?token=fixture-token" }
    foreach ($name in @('HEART_PORTAL_SUPERVISED','HEART_PORTAL_READY_FILE','HEART_PORTAL_READY_NONCE')) { $info.EnvironmentVariables.Remove($name) }
    $process = [Diagnostics.Process]::Start($info)
    return @{ process = $process; output = $process.StandardOutput.ReadToEndAsync(); errors = $process.StandardError.ReadToEndAsync() }
}
function Complete-PortalCli($Running) {
    $process = $Running.process
    try {
        $output = $Running.output
        $errors = $Running.errors
        if (-not $process.WaitForExit(100000)) { $process.Kill(); throw 'Portal CLI timed out.' }
        Assert ($output.Wait(2000) -and $errors.Wait(2000)) 'CLI streams close after supervisor handoff'
        return [pscustomobject]@{ code=$process.ExitCode; output=$output.Result; error=$errors.Result }
    } finally { $process.Dispose() }
}
function Run-Portal([string[]]$Arguments = @(), [string]$Directory = $testRoot) {
    return (Complete-PortalCli (Start-PortalCli $Arguments $Directory))
}
try {
    Assert (@(Get-ChildItem -Force -LiteralPath $testRoot).Count -eq 1) 'fixture starts with one exe only'
    $version = (Run-Portal @('--version')).output.Trim().Split(' ')[1]
    Assert (@(Get-ChildItem -Force -LiteralPath $testRoot).Count -eq 1) 'version query never bootstraps'
    $first = Start-PortalCli
    $second = Start-PortalCli
    $result = Complete-PortalCli $first
    $secondResult = Complete-PortalCli $second
    Assert ($result.code -eq 0) "first exe launch succeeds: $($result.error)"
    Assert ($secondResult.code -eq 0) "concurrent first launch succeeds: $($secondResult.error)"
    Assert (Test-PortalReady $testRoot $version) 'first run starts a ready Portal'
    $initial = Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')
    Assert ($initial.supervisor_pid -and $initial.bootstrap_pid) 'both supervisor levels start automatically'
    Assert (Test-Path -LiteralPath (Join-Path $testRoot 'portal.toml')) 'default config is created'
    $startStatus = Read-PortalJson (Join-Path $testRoot '.portal-start-status.json')
    Assert $startStatus.autostart "logon recovery registered: $($startStatus.warning)"
    $taskName = Get-PortalSavedValue $testRoot '.portal-task-name'
    Assert-PortalTaskOwnership $testRoot $taskName
    Assert ((Get-ScheduledTask -TaskName $taskName).Actions[0].Execute -like '*powershell.exe') 'no VBScript prerequisite'
    if ($LocalOnly) {
        $socket = [Net.Sockets.TcpClient]::new()
        try { $socket.Connect('127.0.0.1', 9100); Assert $socket.Connected 'blank configuration starts the local MCP listener' }
        finally { $socket.Dispose() }
        Write-Output 'PASS: no connection link or config needed to start the local Portal'
    }
    $launchBefore = [IO.File]::ReadAllText((Join-Path $testRoot '.portal-launch.json'))
    $configBefore = [IO.File]::ReadAllText((Join-Path $testRoot 'portal.toml'))
    $result = Run-Portal @() $tempBase
    Assert ($result.code -eq 0) "repeat launch succeeds: $($result.error)"
    Assert ((Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')).pid -eq $initial.pid) 'repeat launch reuses the process'
    Assert ([IO.File]::ReadAllText((Join-Path $testRoot '.portal-launch.json')) -eq $launchBefore) 'another cwd cannot change saved launch settings'
    $status = (Run-Portal @('status')).output | ConvertFrom-Json
    Assert ($status.ready -and $status.supervised) 'public status confirms supervision'
    $process = Get-Process -Id $initial.pid
    try { $process.Kill(); Assert ($process.WaitForExit(5000)) 'runtime killed for recovery test' } finally { $process.Dispose() }
    Wait-Until { (Test-PortalReady $testRoot $version) -and (Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')).pid -ne $initial.pid } 'real crash recovery' 30
    Write-Output 'PASS: one exe creates config/scripts/logon task; duplicate launch, status and real crash recovery work'
    $beforeUpgrade = Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')
    if ($Candidate) {
        $nextVersion = (& $Candidate --version).Trim().Split(' ')[1]
        $result = Run-Portal @('upgrade','--file',(Resolve-Path -LiteralPath $Candidate).Path)
        Assert ($result.code -eq 0) "public upgrade accepts newer real exe: $($result.error)"
        Wait-Until { (Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -in @('succeeded','rolled_back','failed','recovery_required') } 'upgrade completes'
        $status = Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')
        Assert ($status.state -eq 'succeeded') "real cross-version upgrade succeeds: $($status.message)"
        $version = $nextVersion
        Write-Output 'PASS: single-exe first run -> public upgrade CLI -> higher-version real Portal'
    } else {
        $stage = Join-Path $testRoot ('.portal-upgrades\' + [guid]::NewGuid().ToString('N'))
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        $candidatePath = Join-Path $stage 'heart-portal.exe'
        Copy-Item -LiteralPath $Binary -Destination $candidatePath
        foreach ($name in @('portal-lifecycle.ps1','portal-upgrade-worker.ps1')) { Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $stage }
        $request = @{ root=$testRoot; target=$exe; candidate=$candidatePath; version=$version; sha256='invalid'; parent_pid=[int]::MaxValue; ack=(Join-Path $stage 'accepted.json'); error=(Join-Path $stage 'error.json') }
        Write-PortalJson (Join-Path $stage 'request.json') $request
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + (ConvertTo-PortalArgument (Join-Path $stage 'portal-upgrade-worker.ps1'))
        $info.UseShellExecute = $false; $info.CreateNoWindow = $true
        $worker = [Diagnostics.Process]::Start($info)
        Assert ($worker.WaitForExit(10000) -and $worker.ExitCode -ne 0) 'checksum failure rejected'
        $worker.Dispose(); $worker = $null
        Assert ((Test-PortalReady $testRoot $version) -and (Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')).pid -eq $beforeUpgrade.pid) 'bad download leaves original runtime alive'
        $request.sha256 = (Get-FileHash -LiteralPath $candidatePath).Hash
        Write-PortalJson (Join-Path $stage 'request.json') $request
        [IO.File]::Delete($request.error)
        $worker = [Diagnostics.Process]::Start($info)
        Assert ($worker.WaitForExit(45000) -and $worker.ExitCode -eq 0) 'real exe replacement succeeds'
        $status = (Run-Portal @('upgrade','--status')).output | ConvertFrom-Json
        Assert ($status.state -eq 'succeeded') 'public CLI reports successful replacement'
    }
    Assert (Test-PortalReady $testRoot $version) 'upgraded process remains supervised and ready'
    Assert ([IO.File]::ReadAllText((Join-Path $testRoot '.portal-launch.json')) -eq $launchBefore) 'upgrade preserves launch settings'
    Assert ([IO.File]::ReadAllText((Join-Path $testRoot 'portal.toml')) -eq $configBefore) 'upgrade preserves config'
    Assert ((Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')).pid -ne $beforeUpgrade.pid) 'upgraded runtime has a new PID'
    $result = Run-Portal @('stop')
    Assert ($result.code -eq 0) "stop succeeds: $($result.error)"
    Assert (-not (Test-PortalReady $testRoot)) 'stop leaves no running Portal'
    Assert (-not (Get-ScheduledTask -TaskName $taskName).Settings.Enabled) 'stop disables logon recovery'
    $result = Run-Portal
    Assert ($result.code -eq 0 -and (Test-PortalReady $testRoot $version)) "exe resumes supervision: $($result.error)"
    Assert ((Get-ScheduledTask -TaskName $taskName).Settings.Enabled) 'restart restores logon recovery'
    Write-Output 'PASS: upgraded single exe preserves settings and supports stop/resume with logon recovery'
} catch {
    $testFailure = $_
    Write-Output ("FAILED: " + $_.ToString() + "`n" + $_.ScriptStackTrace)
    throw
} finally {
    if ($worker) { if (-not $worker.HasExited) { $worker.Kill(); $worker.WaitForExit() }; $worker.Dispose() }
    $taskName = Get-PortalSavedValue $testRoot '.portal-task-name'
    if ($taskName) {
        Assert-PortalTaskOwnership $testRoot $taskName
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    }
    # The public CLI launches a detached worker. Stop that exact fixture worker
    # before cleanup so it cannot still hold locks or recover deleted files.
    $stages = Join-Path $testRoot '.portal-upgrades'
    if (Test-Path -LiteralPath $stages) {
        foreach ($stage in @(Get-ChildItem -LiteralPath $stages -Directory)) {
            $scriptPath = Join-Path $stage.FullName 'portal-upgrade-worker.ps1'
            foreach ($item in @(Get-CimInstance Win32_Process | Where-Object {
                $_.Name -in @('powershell.exe','pwsh.exe') -and (Test-PortalScriptCommand $_.CommandLine $scriptPath)
            })) {
                $process = Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
                if ($process) {
                    try { $process.Kill(); [void]$process.WaitForExit(10000) } finally { $process.Dispose() }
                }
            }
        }
    }
    Stop-PortalCheckoutProcesses $testRoot
    $resolved = [IO.Path]::GetFullPath($testRoot)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -notlike 'portal single exe test *') { throw 'Unsafe test cleanup path' }
    if ($testFailure) { Write-Output "Failed fixture retained for diagnosis: $resolved" }
    else { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
