# Run in Windows PowerShell 5.1. No Pester, relay, or real scheduled task needed.
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$tempBase = [IO.Path]::GetTempPath()
$testRoot = Join-Path $tempBase ("portal Windows test " + [char]0x6D4B + '-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $testRoot 'target\release') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $testRoot 'scripts') | Out-Null
$supervisorDiagnostics = [Collections.Generic.List[object]]::new()
Copy-Item -LiteralPath (Join-Path $repo 'portal.example.toml') -Destination $testRoot
foreach ($script in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-bootstrap.ps1', 'portal-supervisor-hidden.vbs', 'portal-task-common.ps1', 'install-portal-task.ps1', 'install-portal-windows.ps1', 'uninstall-portal-task.ps1')) {
    Copy-Item -LiteralPath (Join-Path $repo "scripts\$script") -Destination (Join-Path $testRoot 'scripts')
}

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Assertion failed: $Message" }
}
function Wait-Until([scriptblock]$Condition, [string]$Message, [int]$TimeoutSeconds = 20) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds)
    throw "Timed out: $Message (test files: $testRoot)"
}
function Launch-Supervisor {
    $supervisor = Join-Path $testRoot 'scripts\portal-supervisor-bootstrap.ps1'
    $arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}" -Root "{1}" -RestartDelaySeconds 1' -f $supervisor, $testRoot
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $info.Arguments = $arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $child = [Diagnostics.Process]::Start($info)
    $supervisorDiagnostics.Add(@{ Output = $child.StandardOutput.ReadToEndAsync(); Error = $child.StandardError.ReadToEndAsync() })
    return $child
}
function Get-Launches {
    $path = Join-Path $testRoot 'launches.txt'
    if (Test-Path -LiteralPath $path) { return @(Get-Content -LiteralPath $path) }
    return @()
}

try {
    $fixture = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'fake-portal.cs') -Raw
    Add-Type -TypeDefinition $fixture -OutputAssembly (Join-Path $testRoot 'target\release\heart-portal.exe') -OutputType ConsoleApplication

    # Mocks are confined to this scope, so real tests below can inspect only the
    # fixture's processes. No scheduler registration or system process kill runs.
    & {
        $global:PortalTestTasks = @{}
        $global:PortalTestFailRegistration = $false
        $global:PortalTestStarted = @()
        function Get-ScheduledTask { [CmdletBinding()] param($TaskName); return $global:PortalTestTasks[$TaskName] }
        function New-ScheduledTaskAction { param($Execute, $Argument, $WorkingDirectory); return [pscustomobject]@{ Execute = $Execute; Arguments = $Argument; WorkingDirectory = $WorkingDirectory } }
        function New-ScheduledTaskTrigger { param([switch]$AtLogOn, $User); return [pscustomobject]@{ User = $User } }
        function New-ScheduledTaskPrincipal { param($UserId, $LogonType, $RunLevel); return [pscustomobject]@{ UserId = $UserId; LogonType = $LogonType; RunLevel = $RunLevel } }
        function New-ScheduledTaskSettingsSet {
            param([switch]$StartWhenAvailable, [switch]$AllowStartIfOnBatteries, [switch]$DontStopIfGoingOnBatteries, $MultipleInstances, $ExecutionTimeLimit, $RestartCount, $RestartInterval)
            return [pscustomobject]@{ MultipleInstances = $MultipleInstances; ExecutionTimeLimit = $ExecutionTimeLimit }
        }
        function Register-ScheduledTask {
            param($TaskName, $Action, $Trigger, $Settings, $Principal, $Description, [switch]$Force)
            if ($global:PortalTestFailRegistration) { throw 'Simulated access denied' }
            $global:PortalTestTasks[$TaskName] = [pscustomobject]@{ Actions = @($Action); Settings = $Settings; Principal = $Principal }
        }
        function Stop-ScheduledTask { [CmdletBinding()] param($TaskName) }
        function Start-ScheduledTask { param($TaskName); $global:PortalTestStarted += $TaskName }
        function Unregister-ScheduledTask { [CmdletBinding()] param($TaskName, [switch]$Confirm); $global:PortalTestTasks.Remove($TaskName) }
        function Get-CimInstance { param($ClassName); return @() }

        $installer = Join-Path $testRoot 'scripts\install-portal-windows.ps1'
        $taskInstaller = Join-Path $testRoot 'scripts\install-portal-task.ps1'
        $link = 'https://relay.invalid/test-being/?token=fake-test-token'
        $failed = $false
        try { & $installer -Root $testRoot -ConnectLink 'https://relay.invalid/test-being/' } catch { $failed = $true }
        Assert ($failed -and $global:PortalTestTasks.Count -eq 0) 'invalid link fails before installation'
        $global:PortalTestFailRegistration = $true
        $failed = $false
        try { & $installer -Root $testRoot -ConnectLink $link -PortalName 'first-name' } catch { $failed = $true }
        Assert $failed 'first registration failure reported'
        Assert (-not (Test-Path -LiteralPath (Join-Path $testRoot '.portal-connection.url'))) 'failed first registration does not save connection'
        $global:PortalTestFailRegistration = $false
        & $installer -Root $testRoot -ConnectLink $link -PortalName 'first-name' -TaskName 'CustomPortalTask'
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'explicit first name'
        Assert (Test-Path -LiteralPath (Join-Path $testRoot 'workspace')) 'first install creates workspace'
        $config = Get-Content -LiteralPath (Join-Path $testRoot 'portal.toml') -Raw
        & $installer -Root $testRoot -ConnectLink ($link.Replace('fake-test-token', 'rotated-test-token'))
        & $taskInstaller -Root $testRoot
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'reinstall preserves name'
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('CustomPortalTask')) 'reinstall preserves custom task'
        Assert ((Get-Content (Join-Path $testRoot 'portal.toml') -Raw) -eq $config) 'reinstall preserves config'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Actions[0].Execute -like '*\wscript.exe') 'task is windowless'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Settings.MultipleInstances -eq 'IgnoreNew') 'scheduler prevents duplicates'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Principal.LogonType -eq 'Interactive') 'task uses installing user'

        $global:PortalTestFailRegistration = $true
        $failed = $false
        try { & $taskInstaller -Root $testRoot -PortalName 'must-not-persist' } catch { $failed = $true }
        Assert $failed 'registration failure reported'
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'failed install keeps identity'
        $savedLink = [IO.File]::ReadAllText((Join-Path $testRoot '.portal-connection.url'))
        $failed = $false
        try { & $installer -Root $testRoot -ConnectLink $link -PortalName 'must-not-persist' } catch { $failed = $true }
        Assert $failed 'outer installer reports registration failure'
        Assert ([IO.File]::ReadAllText((Join-Path $testRoot '.portal-connection.url')) -eq $savedLink) 'failed registration preserves connection'
        $global:PortalTestFailRegistration = $false

        & $taskInstaller -Root $testRoot -TaskName 'RenamedPortalTask'
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('RenamedPortalTask')) 'explicit task rename removes old task'
        $global:PortalTestTasks['UnrelatedTask'] = [pscustomobject]@{ Actions = @([pscustomobject]@{ Arguments = 'unrelated.ps1' }) }
        $failed = $false
        try { & $taskInstaller -Root $testRoot -TaskName 'UnrelatedTask' } catch { $failed = $true }
        Assert $failed 'cannot overwrite unrelated task'
        $failed = $false
        try { & $installer -Root $testRoot -ConnectLink $link -TaskName 'UnrelatedTask' } catch { $failed = $true }
        Assert $failed 'outer installer rejects unrelated task'
        Assert ([IO.File]::ReadAllText((Join-Path $testRoot '.portal-connection.url')) -eq $savedLink) 'task conflict preserves connection'
        & (Join-Path $testRoot 'scripts\uninstall-portal-task.ps1') -Root $testRoot
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('UnrelatedTask')) 'uninstall only removes owned task'
        Write-Output 'PASS: install, reinstall, name/task persistence, hidden action, failed registration, rename, uninstall'
    }

    . (Join-Path $testRoot 'scripts\portal-task-common.ps1')
    $scriptPath = Join-Path $testRoot 'scripts\portal-supervisor.ps1'
    Assert (Test-PortalScriptCommand ('-File "{0}"' -f $scriptPath) $scriptPath) 'quoted exact script matches'
    Assert (-not (Test-PortalScriptCommand ('-File "{0}.backup"' -f $scriptPath) $scriptPath)) 'similar script does not match'
    $supervisorProcess = Launch-Supervisor
    Wait-Until { @(Get-Launches).Count -ge 1 } 'first Portal start'
    $duplicate = Launch-Supervisor
    try {
        Assert ($duplicate.WaitForExit(10000)) 'duplicate supervisor exits'
        Assert ($duplicate.ExitCode -eq 0) 'duplicate supervisor reports success without spawning'
    } finally { $duplicate.Dispose() }
    Assert (@(Get-Launches).Count -eq 1) 'only one Portal created'

    $launches = @(Get-Launches)
    $portalPid = [int]$launches[0].Split('|')[0]
    $portalProcess = Get-Process -Id $portalPid
    try {
        Assert ($portalProcess.Path -eq (Join-Path $testRoot 'target\release\heart-portal.exe')) 'kill target is fixture'
        $portalProcess.Kill()
        Assert ($portalProcess.WaitForExit(5000)) 'fixture exits after kill'
    } finally { $portalProcess.Dispose() }
    Wait-Until { @(Get-Launches).Count -ge 2 } 'Portal restarts after external kill'
    foreach ($launch in @(Get-Launches)) {
        Assert ($launch -like '*|--name|first-name|1') 'restart keeps original name and supervised marker'
    }
    Assert (-not $supervisorProcess.HasExited) 'supervisor survives Portal kill'
    Write-Output 'PASS: duplicate supervisor rejected; crash restart keeps original name'

    $gate = Open-PortalLock $testRoot '.portal-lifecycle.lock' 15
    try {
        $countBefore = @(Get-Launches).Count
        Stop-PortalRuntime $testRoot
        Start-Sleep -Seconds 3
        Assert (@(Get-Launches).Count -eq $countBefore) 'maintenance gate prevents a relaunch after the runtime exits'
    } finally { $gate.Dispose() }
    Wait-Until { @(Get-Launches).Count -gt $countBefore } 'restart after maintenance releases its handle'
    Write-Output 'PASS: supervisor cannot race the maintenance gate'

    function Launch-Upgrade([string]$Version, [switch]$FailStartup, [switch]$FailSupervisor) {
        $stage = Join-Path $testRoot ('.portal-upgrades\' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $stage -Force | Out-Null
        foreach ($name in @('portal-lifecycle.ps1', 'portal-upgrade-worker.ps1')) {
            Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $stage
        }
        $candidate = Join-Path $stage 'heart-portal.exe'
        $fixtureSupport = Join-Path $stage 'fixture-support'
        New-Item -ItemType Directory -Path $fixtureSupport | Out-Null
        foreach ($name in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-hidden.vbs', 'portal-supervisor-bootstrap.ps1')) {
            Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $fixtureSupport
        }
        Add-Content -LiteralPath (Join-Path $fixtureSupport 'portal-supervisor.ps1') -Value "# Fixture supervisor version $Version"
        if ($FailSupervisor) { [IO.File]::WriteAllText((Join-Path $fixtureSupport 'portal-supervisor.ps1'), 'exit 0') }
        $code = $fixture.Replace('0.8.0', $Version)
        if ($FailStartup) { $code = $code.Replace('string root = Environment.CurrentDirectory;', 'return 23; /*').Replace('while (true) { Thread.Sleep(100); }', '*/') }
        Add-Type -TypeDefinition $code -OutputAssembly $candidate -OutputType ConsoleApplication
        $request = @{
            root = $testRoot; target = (Get-PortalExecutable $testRoot); candidate = $candidate
            version = $Version; sha256 = (Get-FileHash -LiteralPath $candidate).Hash
            parent_pid = [int]::MaxValue; ack = (Join-Path $stage 'accepted.json'); error = (Join-Path $stage 'error.json')
        }
        Write-PortalJson (Join-Path $stage 'request.json') $request
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + (ConvertTo-PortalArgument (Join-Path $stage 'portal-upgrade-worker.ps1'))
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        return [Diagnostics.Process]::Start($info)
    }
    $oldHash = (Get-FileHash -LiteralPath (Get-PortalExecutable $testRoot)).Hash
    $upgradeProcess = Launch-Upgrade '0.8.1'
    try {
        Wait-Until { (Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'verifying' } 'upgrade reaches supervised verification'
        $failed = $false
        try { $locks = Enter-PortalMaintenance $testRoot; foreach ($lock in $locks) { $lock.Dispose() } } catch { $failed = $true }
        Assert $failed 'install/uninstall rejected while upgrade owns transaction'
        $duplicateUpgrade = Launch-Upgrade '0.8.2'
        try { Assert ($duplicateUpgrade.WaitForExit(10000) -and $duplicateUpgrade.ExitCode -ne 0) 'concurrent upgrade rejected' }
        finally { $duplicateUpgrade.Dispose() }
        Assert ($upgradeProcess.WaitForExit(30000) -and $upgradeProcess.ExitCode -eq 0) 'upgrade worker succeeds'
        Assert ((Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'succeeded') 'upgrade reports committed success'
        Assert (Test-PortalReady $testRoot '0.8.1') 'new version is running and locally ready'
        Assert ((Get-FileHash -LiteralPath (Get-PortalExecutable $testRoot)).Hash -ne $oldHash) 'running exe was replaced'
        Assert (-not (Test-Path -LiteralPath (Join-Path $testRoot '.portal-upgrade.json'))) 'successful upgrade commits journal'
    } finally { if (-not $upgradeProcess.HasExited) { $upgradeProcess.Kill() }; $upgradeProcess.Dispose() }
    Write-Output 'PASS: upgrade replaces the actual executable, blocks concurrent maintenance/upgrades, and verifies the new version'

    $goodHash = (Get-FileHash -LiteralPath (Get-PortalExecutable $testRoot)).Hash
    $goodSupervisorHash = (Get-FileHash -LiteralPath (Join-Path $testRoot 'scripts\portal-supervisor.ps1')).Hash
    $upgradeProcess = Launch-Upgrade '0.8.3' -FailStartup
    try {
        Wait-Until { (Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'verifying' } 'broken binary reaches runtime verification'
        $upgradeProcess.Kill()
        Assert ($upgradeProcess.WaitForExit(5000)) 'interrupted updater exits'
        Wait-Until { (Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'rolled_back' -and (Test-PortalReady $testRoot '0.8.1') } 'supervisor recovers interrupted upgrade' 30
        Assert ((Get-FileHash -LiteralPath (Get-PortalExecutable $testRoot)).Hash -eq $goodHash) 'interrupted upgrade restores exact previous binary'
        Assert ((Get-FileHash -LiteralPath (Join-Path $testRoot 'scripts\portal-supervisor.ps1')).Hash -eq $goodSupervisorHash) 'interrupted upgrade restores matching supervisor code'
    } finally { if (-not $upgradeProcess.HasExited) { $upgradeProcess.Kill() }; $upgradeProcess.Dispose() }
    Write-Output 'PASS: killed updater releases its OS lock and supervisor restores the previous binary'

    $upgradeProcess = Launch-Upgrade '0.8.4' -FailStartup
    try {
        Assert ($upgradeProcess.WaitForExit(90000) -and $upgradeProcess.ExitCode -ne 0) 'startup failure rolls back and reports failure'
        Assert ((Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'rolled_back') 'startup failure records rollback'
        Assert (Test-PortalReady $testRoot '0.8.1') 'old version is ready after failed upgrade'
    } finally { if (-not $upgradeProcess.HasExited) { $upgradeProcess.Kill() }; $upgradeProcess.Dispose() }
    Write-Output 'PASS: startup timeout rolls back and verifies the old version'

    $upgradeProcess = Launch-Upgrade '0.8.5' -FailSupervisor
    try {
        Assert ($upgradeProcess.WaitForExit(90000) -and $upgradeProcess.ExitCode -ne 0) 'broken supervisor rolls back and reports failure'
        Assert ((Read-PortalJson (Join-Path $testRoot '.portal-upgrade-status.json')).state -eq 'rolled_back') 'broken supervisor records rollback'
        Assert (Test-PortalReady $testRoot '0.8.1') 'old Portal and supervisor work after supervisor update failure'
        Assert ((Get-FileHash -LiteralPath (Join-Path $testRoot 'scripts\portal-supervisor.ps1')).Hash -eq $goodSupervisorHash) 'broken supervisor restores the exact previous script'
    } finally { if (-not $upgradeProcess.HasExited) { $upgradeProcess.Kill() }; $upgradeProcess.Dispose() }
    Write-Output 'PASS: supervisor update failure restores the matching Portal and supervisor as one transaction'

    Stop-PortalCheckoutProcesses $testRoot
    $supervisorProcess.Dispose()

    New-Item -ItemType File -Path (Join-Path $testRoot 'hold-pipes') | Out-Null
    $countBefore = @(Get-Launches).Count
    $supervisorProcess = Launch-Supervisor
    Wait-Until {
        $log = Join-Path $testRoot 'portal-runtime.log'
        (Test-Path -LiteralPath $log) -and ((Get-Content -LiteralPath $log -Raw) -like '*child holding inherited stdout*')
    } 'fixture child really inherited stdout'
    Wait-Until { @(Get-Launches).Count -ge ($countBefore + 2) } 'restart while kit holds inherited log pipes' 15
    Write-Output 'PASS: inherited stdout/stderr cannot stall supervisor restart'
    Stop-PortalCheckoutProcesses $testRoot
    $supervisorProcess.Dispose()
} finally {
    . (Join-Path $testRoot 'scripts\portal-task-common.ps1')
    Stop-PortalCheckoutProcesses $testRoot
    $resolvedTestRoot = (Resolve-Path -LiteralPath $testRoot).Path
    foreach ($diagnostic in $supervisorDiagnostics) {
        if ($diagnostic.Output.Wait(2000)) { Write-Output $diagnostic.Output.Result }
        if ($diagnostic.Error.Wait(2000)) { Write-Output $diagnostic.Error.Result }
    }
    if (-not $resolvedTestRoot.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path $resolvedTestRoot -Leaf) -notlike 'portal Windows test *') { throw 'Unsafe test cleanup path' }
    Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
}
