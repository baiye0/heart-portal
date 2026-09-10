# Exercise an unmodified downloaded release, with isolated tasks and user data.
param(
    [Parameter(Mandatory=$true)][string]$Baseline,
    [Parameter(Mandatory=$true)][string]$Candidate,
    [ValidateSet('managed','legacy')][string]$Layout = 'managed',
    [string]$Report = ''
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$Baseline = (Resolve-Path -LiteralPath $Baseline).Path
$Candidate = (Resolve-Path -LiteralPath $Candidate).Path
$oldVersion = (& $Baseline --version).Trim().Split(' ')[1]
$newVersion = (& $Candidate --version).Trim().Split(' ')[1]
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('portal-release-upgrade-' + [guid]::NewGuid().ToString('N'))
$profile = Join-Path $fixture 'user'
$data = Join-Path $profile '.heart-portal'
$managed = Join-Path $data 'runtime'
$legacy = Join-Path $fixture 'legacy'
$root = if ($Layout -eq 'legacy') { $legacy } else { $managed }
[void][IO.Directory]::CreateDirectory($root)
$exe = Join-Path $root 'heart-portal.exe'
Copy-Item -LiteralPath $Baseline -Destination $exe
$config = if ($Layout -eq 'legacy') { Join-Path $root 'portal.toml' } else { Join-Path $data 'portal.toml' }
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop()
$utf8 = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($config, "name='release-fixture'`nbind='127.0.0.1:$port'`nworkspace='./workspace'`nkits_dir='./kits'`nkits_enabled=false`n", $utf8)
$workspace = Join-Path ([IO.Path]::GetDirectoryName($config)) 'workspace'
$kits = Join-Path ([IO.Path]::GetDirectoryName($config)) 'kits'
[void][IO.Directory]::CreateDirectory($workspace)
[void][IO.Directory]::CreateDirectory($kits)
[IO.File]::WriteAllText((Join-Path $workspace 'keep.txt'), 'existing workspace')
[IO.File]::WriteAllText((Join-Path $kits '.env'), 'existing kit credentials')
$token = 'isolated-release-mcp-token'
$supplyToken = $true
$result = [ordered]@{
    layout=$Layout; baseline_version=$oldVersion; candidate_version=$newVersion
    baseline_sha256=(Get-FileHash -LiteralPath $Baseline).Hash
    candidate_sha256=(Get-FileHash -LiteralPath $Candidate).Hash
    checks=@(); passed=$false
}
function Assert([bool]$Value, [string]$Message) {
    if (-not $Value) { throw $Message }
}
function Check([string]$Message) { $result.checks += $Message; Write-Output "PASS: $Message" }
function Wait-Until([scriptblock]$Predicate, [string]$Message, [int]$Seconds=120) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $Seconds) {
        if (& $Predicate) { return }
        Start-Sleep -Milliseconds 200
    }
    throw "Timed out: $Message"
}
function Run-Portal([string[]]$Arguments=@()) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName=$exe; $info.Arguments=(@($Arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
    $info.WorkingDirectory=$root; $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    foreach ($key in @('HEART_PORTAL_SUPERVISED','HEART_PORTAL_READY_FILE','HEART_PORTAL_READY_NONCE','HEART_PORTAL_EXTERNAL_TOOL')) { $info.EnvironmentVariables.Remove($key) }
    $info.EnvironmentVariables['HOME']=$profile; $info.EnvironmentVariables['USERPROFILE']=$profile
    $info.EnvironmentVariables['PORTAL_CONNECT_LINK']=''
    $info.EnvironmentVariables['PORTAL_MCP_TOKEN']=if ($supplyToken) { $token } else { '' }
    $process=[Diagnostics.Process]::Start($info)
    try {
        $out=$process.StandardOutput.ReadToEndAsync(); $err=$process.StandardError.ReadToEndAsync()
        Assert ($process.WaitForExit(100000)) 'CLI did not finish'
        Assert ($out.Wait(2000) -and $err.Wait(2000)) 'CLI inherited pipes stayed open'
        Assert (-not ($out.Result+$err.Result).Contains($token)) 'CLI exposed the auth token'
        Assert ($process.ExitCode -eq 0) ('CLI failed: '+$err.Result)
        return $out.Result
    } finally {
        if (-not $process.HasExited) { $process.Kill(); [void]$process.WaitForExit(5000) }
        $process.Dispose()
    }
}
function Assert-Authentication {
    foreach ($valid in @($false,$true)) {
        $client=[Net.Sockets.TcpClient]::new('127.0.0.1',$port)
        try {
            $stream=$client.GetStream(); $stream.ReadTimeout=10000
            $writer=[IO.StreamWriter]::new($stream,$utf8,1024,$true); $writer.AutoFlush=$true
            $reader=[IO.StreamReader]::new($stream,$utf8,$false,1024,$true)
            try {
                $credential=if ($valid) { $token } else { 'wrong' }
                $writer.WriteLine((@{jsonrpc='2.0';id=1;method='auth';params=@{token=$credential}} | ConvertTo-Json -Compress -Depth 4))
                $reply=$reader.ReadLine() | ConvertFrom-Json
                if ($valid) { Assert ($reply.result.authenticated -eq $true) 'Saved MCP authentication was lost' }
                else { Assert ($reply.error.code -eq -32002) 'Unauthenticated access was accepted' }
            } finally { $writer.Dispose(); $reader.Dispose() }
        } finally { $client.Dispose() }
    }
}
try {
    Run-Portal @('--config',$config,'--name','release-fixture') | Out-Null
    Assert (Test-PortalReady $root $oldVersion) 'Downloaded release did not start'
    Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $result.baseline_sha256) 'Baseline bytes changed'
    $originalConfig=[IO.File]::ReadAllBytes($config)
    $originalLaunch=[IO.File]::ReadAllText((Join-Path $root '.portal-launch.json'))
    $oldRuntime=Read-PortalJson (Join-Path $root '.portal-runtime.json')
    Assert-Authentication
    Check 'Unmodified GitHub release starts with working authentication and supervision'
    if ($Layout -eq 'legacy') {
        $rejected=$false
        try { Run-Portal @('upgrade','--file',$Candidate) | Out-Null }
        catch { Assert ($_.Exception.Message -like '*Upgrade rejected*') 'Unexpected legacy upgrade failure'; $rejected=$true }
        Assert $rejected 'Unsafe legacy directory upgrade was accepted'
        $result.direct_upgrade='rejected_before_stop'
        Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $result.baseline_sha256) 'Rejected upgrade changed official bytes'
        Assert ((Test-PortalReady $root $oldVersion) -and (Test-SavedSupervisor (Read-PortalJson (Join-Path $root '.portal-runtime.json')))) 'Rejected upgrade interrupted the original Portal'
        $unchanged=Read-PortalJson (Join-Path $root '.portal-runtime.json')
        Assert ($unchanged.pid -eq $oldRuntime.pid -and $unchanged.supervisor_pid -eq $oldRuntime.supervisor_pid) 'Rejected upgrade restarted Portal or guardian'
        Assert (-not (Test-Path -LiteralPath (Join-Path $root '.portal-upgrade.json'))) 'Rejected upgrade left a recovery journal'
        Wait-Until {
            $released=Open-PortalLock $root '.portal-upgrade.lock'
            if ($released) { $released.Dispose(); return $true }
            return $false
        } 'old updater releases rejected transaction' 30
        Assert ([IO.File]::ReadAllText((Join-Path $root '.portal-launch.json')) -eq $originalLaunch) 'Rollback changed launch settings'
        Assert-Authentication
        Check 'Old-directory upgrade is rejected before stopping; original PID, guardian, auth and binary remain intact'
        Run-Portal @('stop') | Out-Null
        $oldTask=Get-PortalSavedValue $root '.portal-task-name'
        Assert (-not (Get-ScheduledTask -TaskName $oldTask).Settings.Enabled) 'Legacy login task remains enabled'
        $exe=Join-Path $root 'heart-portal-windows-x86_64.exe'
        Copy-Item -LiteralPath $Candidate -Destination $exe
        $oldFiles=@(Get-ChildItem -LiteralPath $legacy -Force | ForEach-Object Name)
        $supplyToken=$false
        Run-Portal | Out-Null
        Assert (Test-PortalReady $managed $newVersion) 'Stopped legacy installation did not migrate'
        Assert (-not (Compare-Object $oldFiles @(Get-ChildItem -LiteralPath $legacy -Force | ForEach-Object Name))) 'Migration generated files beside download'
        Assert ([Convert]::ToBase64String([IO.File]::ReadAllBytes($config)) -eq [Convert]::ToBase64String($originalConfig)) 'Migration modified original config'
        $root=$managed
        Check 'Stopped legacy installation migrates into USERPROFILE without modifying source config'
    } else {
        Run-Portal @('upgrade','--file',$Candidate) | Out-Null
        Wait-Until { (Read-PortalJson (Join-Path $root '.portal-upgrade-status.json')).state -in @('succeeded','rolled_back','failed','recovery_required') } 'release upgrade outcome'
        $state=(Read-PortalJson (Join-Path $root '.portal-upgrade-status.json')).state
        $result.direct_upgrade=$state
        Assert ($state -eq 'succeeded') "Official release upgrade failed: $state"
        Assert ([IO.File]::ReadAllText((Join-Path $root '.portal-launch.json')) -eq $originalLaunch) 'Upgrade changed launch settings'
        Assert ([Convert]::ToBase64String([IO.File]::ReadAllBytes($config)) -eq [Convert]::ToBase64String($originalConfig)) 'Upgrade changed config'
        Check 'Official release public upgrade --file installs the local candidate successfully'
    }
    Assert (Test-PortalReady $root $newVersion) 'Candidate is not locally ready'
    Assert ((Get-FileHash -LiteralPath (Get-PortalExecutable $root)).Hash -eq $result.candidate_sha256) 'Running image differs from delivered candidate'
    Assert-Authentication
    Assert ([IO.File]::ReadAllText((Join-Path $workspace 'keep.txt')) -eq 'existing workspace') 'Workspace data changed'
    Assert ([IO.File]::ReadAllText((Join-Path $kits '.env')) -eq 'existing kit credentials') 'Kit credentials changed'
    Check 'Delivered image hash, MCP auth, workspace and kit credentials are preserved'
    $before=Read-PortalJson (Join-Path $root '.portal-runtime.json')
    $process=Get-Process -Id $before.pid
    try { $process.Kill(); [void]$process.WaitForExit(5000) } finally { $process.Dispose() }
    Wait-Until { (Test-PortalReady $root $newVersion) -and (Read-PortalJson (Join-Path $root '.portal-runtime.json')).pid -ne $before.pid } 'candidate crash recovery' 30
    $before=Read-PortalJson (Join-Path $root '.portal-runtime.json')
    $result.before_guardian_crash=$before
    $bootstrap=Get-Process -Id $before.bootstrap_pid -ErrorAction SilentlyContinue
    Assert ($null -ne $bootstrap) 'Recovery bootstrap exited before guardian crash'
    $bootstrap.Dispose()
    Stop-Process -Id $before.supervisor_pid -Force
    Wait-Until { $now=Read-PortalJson (Join-Path $root '.portal-runtime.json'); (Test-SavedSupervisor $now) -and $now.supervisor_pid -ne $before.supervisor_pid } 'guardian adoption' 30
    Assert ((Read-PortalJson (Join-Path $root '.portal-runtime.json')).pid -eq $before.pid) 'Guardian recovery replaced a healthy Portal'
    Check 'Candidate crash restarts; guardian crash adopts the healthy runtime'
    Run-Portal @('stop') | Out-Null
    Assert (-not (Test-PortalReady $root)) 'Stop left a live runtime'
    Run-Portal | Out-Null
    Assert (Test-PortalReady $root $newVersion) 'Resume did not restore supervision'
    Assert-Authentication
    Check 'Stop and resume retain authentication and supervision'
    $result.passed=$true
} catch {
    $result.failure=$_.Exception.Message
    $result.runtime_at_failure=Read-PortalJson (Join-Path $root '.portal-runtime.json')
    $result.processes_at_failure=@(Get-CimInstance Win32_Process | Where-Object {
        $_.CommandLine -and $_.CommandLine.Contains($fixture)
    } | Select-Object ProcessId,ParentProcessId,Name,CommandLine)
    throw
} finally {
    foreach ($owned in @($legacy,$managed)) {
        if (-not (Test-Path -LiteralPath $owned)) { continue }
        $task=Get-PortalSavedValue $owned '.portal-task-name'
        if ($task) {
            Assert-PortalTaskOwnership $owned $task
            Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
        }
        $stages = Join-Path $owned '.portal-upgrades'
        if (Test-Path -LiteralPath $stages) {
            foreach ($stage in @(Get-ChildItem -LiteralPath $stages -Directory)) {
                $scriptPath = Join-Path $stage.FullName 'portal-upgrade-worker.ps1'
                foreach ($item in @(Get-CimInstance Win32_Process | Where-Object {
                    $_.Name -in @('powershell.exe','pwsh.exe') -and (Test-PortalScriptCommand $_.CommandLine $scriptPath)
                })) { Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue }
            }
        }
        Stop-PortalCheckoutProcesses $owned
    }
    if ($Report) {
        [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Report)))
        $result | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $Report -Encoding utf8
    }
    if ($result.passed) {
        $resolved=[IO.Path]::GetFullPath($fixture)
        if ([IO.Path]::GetDirectoryName($resolved) -ne [IO.Path]::GetTempPath().TrimEnd('\') -or [IO.Path]::GetFileName($resolved) -notlike 'portal-release-upgrade-*') { throw 'Unsafe fixture cleanup path' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    } else { Write-Output "Failed fixture retained: $fixture" }
}
