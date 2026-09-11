# Real Kit trees that ignore stdin EOF. Run with Windows PowerShell 5.1.
# Uses an isolated installation, user profile, port and scheduled task.
param(
    [string]$Binary = (Join-Path $PSScriptRoot '..\..\target\debug\heart-portal.exe'),
    [string]$Candidate = ''
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$tempBase = [IO.Path]::GetTempPath()
$root = Join-Path $tempBase ('portal-kit-lifecycle-' + [guid]::NewGuid().ToString('N'))
$fixtureRoot = $root
$profileRoot = Join-Path $fixtureRoot 'profile'
$root = Join-Path $profileRoot '.heart-portal\runtime'
[void][IO.Directory]::CreateDirectory($root)
$exe = Join-Path $root 'heart-portal.exe'
$configPath = Join-Path $profileRoot '.heart-portal\portal.toml'
$python = (Get-Command python -ErrorAction Stop).Source
$utf8 = [Text.UTF8Encoding]::new($false)
$passed = $false
$outsider = $null
function Assert([bool]$Value, [string]$Message) { if (-not $Value) { throw $Message } }
function Wait-Until([scriptblock]$Condition, [string]$Message, [int]$Seconds = 45) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $Seconds) {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out: $Message"
}
function Run-Portal([string[]]$Arguments = @()) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $exe
    $info.Arguments = (@($Arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
    $info.WorkingDirectory = $root
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.EnvironmentVariables['HOME'] = $profileRoot
    $info.EnvironmentVariables['USERPROFILE'] = $profileRoot
    $info.EnvironmentVariables['PORTAL_CONNECT_LINK'] = ''
    $info.EnvironmentVariables['PORTAL_MCP_TOKEN'] = ''
    foreach ($name in @('HEART_PORTAL_SUPERVISED','HEART_PORTAL_READY_FILE','HEART_PORTAL_READY_NONCE','HEART_PORTAL_KIT_JOB')) { $info.EnvironmentVariables.Remove($name) }
    $process = [Diagnostics.Process]::Start($info)
    try {
        $out = $process.StandardOutput.ReadToEndAsync(); $err = $process.StandardError.ReadToEndAsync()
        Assert ($process.WaitForExit(90000)) 'Fixture CLI timed out'
        Assert ($out.Wait(2000) -and $err.Wait(2000)) 'CLI inherited streams did not close'
        Assert ($process.ExitCode -eq 0) ('Fixture CLI failed: ' + $err.Result)
    } finally {
        if (-not $process.HasExited) { $process.Kill(); [void]$process.WaitForExit(5000) }
        $process.Dispose()
    }
}
function Runtime { Read-PortalJson (Join-Path $root '.portal-runtime.json') }
function Kit-Records([int]$Owner = 0) {
    foreach ($file in @(Get-ChildItem -LiteralPath (Join-Path $root 'markers') -Filter '*.json')) {
        $record = Read-PortalJson $file.FullName
        if ($record -and (-not $Owner -or $record.owner -eq $Owner)) { $record }
    }
}
function Assert-KitsGone([int]$Owner) {
    Wait-Until {
        $remaining = @(foreach ($record in @(Kit-Records $Owner)) {
            $process = Get-CimInstance Win32_Process -Filter "ProcessId=$($record.pid)"
            if ($process -and $process.CommandLine -and $process.CommandLine.Contains($root)) { $process }
        })
        $remaining.Count -eq 0
    } 'owned Kit/descendant cleanup' 5
    Assert (-not $outsider.HasExited) 'cleanup killed an independent process'
}
function Wait-KitTree([int]$Owner) {
    Wait-Until { @(Kit-Records $Owner | Where-Object { $_.role -eq 'descendant' }).Count -gt 0 } 'Kit descendant started'
}
function Restart-ThroughMcp {
    $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $port)
    try {
        $stream = $client.GetStream(); $stream.ReadTimeout = 15000
        $writer = [IO.StreamWriter]::new($stream, $utf8, 1024, $true); $writer.AutoFlush = $true
        $reader = [IO.StreamReader]::new($stream, $utf8, $false, 1024, $true)
        try {
            $writer.WriteLine('{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"portal_restart","arguments":{}}}')
            $reply = $reader.ReadLine() | ConvertFrom-Json
            Assert ($reply.id -eq 1 -and -not $reply.error -and -not $reply.result.isError) 'controlled restart did not acknowledge'
        } finally { $reader.Dispose(); $writer.Dispose() }
    } finally { $client.Dispose() }
}
try {
    Copy-Item -LiteralPath $Binary -Destination $exe
    [void][IO.Directory]::CreateDirectory((Join-Path $root 'markers'))
    # Its command line also contains the fixture root, so substring-based tree
    # cleanup would incorrectly kill it. Only explicit test cleanup may do so.
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $python; $info.Arguments = '-c "import time; time.sleep(600)" ' + (ConvertTo-PortalArgument $root)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $outsider = [Diagnostics.Process]::Start($info)
    $source = @'
import json, os, pathlib, subprocess, sys, threading, time
root = pathlib.Path(sys.argv[1])
role = sys.argv[2]
owner = int(sys.argv[3]) if role == "descendant" else os.getppid()
marker = root / "markers" / (str(os.getpid()) + ".json")
temp = marker.with_suffix(".tmp")
temp.write_text(json.dumps(dict(pid=os.getpid(), owner=owner, role=role)))
temp.replace(marker)
if role == "descendant":
    time.sleep(600)  # Ignores stdin EOF; must be killed with its owning Kit.
else:
    subprocess.Popen([sys.executable, __file__, str(root), "descendant", str(owner)], creationflags=0x08000000)
    def upgrade():
        trigger = root / "upgrade-trigger.json"
        while True:
            try:
                request = json.loads(trigger.read_text())
                trigger.rename(root / "upgrade-claimed.json")
                break
            except (OSError, ValueError):
                time.sleep(.05)
        with (root / "kit-upgrade.log").open("w") as output:
            result = subprocess.run([str(root / "heart-portal.exe"), "upgrade", "--file", request["candidate"]],
                           stdout=output, stderr=output, creationflags=0x08000000)
        (root / "kit-upgrade-result.json").write_text(json.dumps(dict(exit_code=result.returncode)))
    threading.Thread(target=upgrade, daemon=True).start()
    if not (root / "fast").exists():
        time.sleep(600)  # Leave MCP initialize pending, like a stuck eager Kit.
    else:
        for line in sys.stdin:
            request = json.loads(line)
            if "id" in request:
                print(json.dumps(dict(jsonrpc="2.0", id=request["id"], result={})), flush=True)
        time.sleep(600)  # Even initialized Kits need reliable forced cleanup.
'@
    for ($n = 0; $n -lt 4; $n++) {
        $kit = Join-Path $root "kits\fixture$n"
        [void][IO.Directory]::CreateDirectory($kit)
        $script = Join-Path $kit 'server.py'
        [IO.File]::WriteAllText($script, $source, $utf8)
        $manifest = @{ name="fixture$n"; version='1.0.0'; eager=$true; command=@($python,$script,$root,'kit'); tools=@(@{ name='test'; description='lifecycle fixture'; params=@{type='object'} }) }
        [IO.File]::WriteAllText((Join-Path $kit 'manifest.json'), ($manifest | ConvertTo-Json -Depth 6), $utf8)
    }
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop()
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($configPath))
    [IO.File]::WriteAllText($configPath, "name='kit-lifecycle-fixture'`nworkspace='./workspace'`nbind='127.0.0.1:$port'`nkits_dir='" + (Join-Path $root 'kits').Replace('\','/') + "'`n", $utf8)

    Run-Portal
    $initial = Runtime
    Assert (Test-PortalReady $root) 'slow Kit blocked local readiness'
    Wait-KitTree $initial.pid
    Run-Portal @('stop')
    Assert-KitsGone $initial.pid
    Start-Sleep -Seconds 3
    Assert (-not (Test-PortalReady $root)) 'guardian restarted an intentionally stopped Portal'
    Write-Output 'PASS: stop during eager initialization kills Kit trees and stays stopped'

    Run-Portal
    $initial = Runtime
    Wait-KitTree $initial.pid
    Restart-ThroughMcp
    Wait-Until { (Test-PortalReady $root) -and (Runtime).pid -ne $initial.pid } 'controlled restart'
    Assert-KitsGone $initial.pid
    Write-Output 'PASS: controlled restart cancels warmup and cleans descendants'

    $initial = Runtime
    Wait-KitTree $initial.pid
    $process = Get-Process -Id $initial.pid
    try { $process.Kill(); Assert ($process.WaitForExit(5000)) 'forced runtime exit' } finally { $process.Dispose() }
    Wait-Until { (Test-PortalReady $root) -and (Runtime).pid -ne $initial.pid } 'guardian recovery after forced exit'
    Assert-KitsGone $initial.pid
    Write-Output 'PASS: forced Portal exit cleans Kit trees; guardian starts a healthy replacement'

    # Fully initialized connections use the same ownership path.
    Run-Portal @('stop')
    [IO.File]::WriteAllText((Join-Path $root 'fast'), '', $utf8)
    Run-Portal
    $initial = Runtime
    Wait-Until { @(Kit-Records $initial.pid | Where-Object { $_.role -eq 'descendant' }).Count -eq 4 } 'all initialized Kit trees'
    $process = Get-Process -Id $initial.supervisor_pid
    try { $process.Kill(); [void]$process.WaitForExit(5000) } finally { $process.Dispose() }
    Wait-Until { (Test-SavedSupervisor (Runtime)) -and (Runtime).supervisor_pid -ne $initial.supervisor_pid } 'supervisor adoption'
    Assert ((Runtime).pid -eq $initial.pid) 'guardian adoption replaced the live Portal'
    foreach ($record in @(Kit-Records $initial.pid)) { Assert ([bool](Get-Process -Id $record.pid -ErrorAction SilentlyContinue)) 'guardian exit killed a live Kit' }
    Write-Output 'PASS: guardian exit retains the healthy Portal and its initialized Kits'

    $fixtureCandidate = if ($Candidate) { (Resolve-Path -LiteralPath $Candidate).Path } else { $exe }
    [IO.File]::WriteAllText((Join-Path $root 'upgrade-trigger.json'), (@{candidate=$fixtureCandidate} | ConvertTo-Json), $utf8)
    Wait-Until { Test-Path -LiteralPath (Join-Path $root 'kit-upgrade-result.json') } 'kit host-management rejection' 15
    $rejectedCall = Read-PortalJson (Join-Path $root 'kit-upgrade-result.json')
    Assert ($rejectedCall.exit_code -ne 0) 'Kit unexpectedly started a Portal upgrade'
    Assert ((Test-PortalReady $root) -and (Runtime).pid -eq $initial.pid) 'Kit upgrade request interrupted Portal'
    Assert (-not (Test-Path -LiteralPath (Join-Path $root '.portal-upgrade-status.json'))) 'Kit started a host upgrade transaction'
    Write-Output 'PASS: managed Kit cannot invoke host upgrade; Portal and its supervisor remain unchanged'
    # Inject a failed post-start health check in this request's private worker,
    # after a real replacement Portal has spawned Kits. Recovery must clean both
    # generations and restore a ready Portal/guardian with fresh Kit processes.
    $initial = Runtime
    Wait-KitTree $initial.pid
    $stage = Join-Path $root ('.portal-upgrades\' + [guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    $stagedExe = Join-Path $stage 'heart-portal.exe'
    Copy-Item -LiteralPath $exe -Destination $stagedExe
    $version = (& $exe --version).Trim().Split(' ')[1]
    foreach ($name in @('portal-lifecycle.ps1','portal-upgrade-worker.ps1')) { Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $stage }
    $fault = @'
$script:originalWait = ${function:Wait-PortalReady}
function Wait-PortalReady([string]$Root, [string]$Version, [int]$TimeoutSeconds = 60) {
    & $script:originalWait -Root $Root -Version $Version -TimeoutSeconds $TimeoutSeconds
    if ($Version) {
        $runtime = Read-PortalJson (Join-Path $Root '.portal-runtime.json')
        $timer = [Diagnostics.Stopwatch]::StartNew()
        do {
            $descendants = @(Get-ChildItem -LiteralPath (Join-Path $Root 'markers') -Filter '*.json' | ForEach-Object { Read-PortalJson $_.FullName } | Where-Object { $_.owner -eq $runtime.pid -and $_.role -eq 'descendant' })
            if ($descendants.Count -eq 4) { break }
            Start-Sleep -Milliseconds 100
        } while ($timer.Elapsed.TotalSeconds -lt 10)
        if ($descendants.Count -ne 4) { throw 'Replacement Kit trees did not start' }
        Write-PortalJson (Join-Path $Root 'rejected-runtime.json') $runtime
        throw 'Injected post-start verification failure'
    }
}
'@
    [IO.File]::AppendAllText((Join-Path $stage 'portal-lifecycle.ps1'), "`r`n" + $fault, $utf8)
    $request = @{ root=$root; target=$exe; candidate=$stagedExe; version=$version; sha256='invalid'; parent_pid=[int]::MaxValue; ack=(Join-Path $stage 'accepted.json'); error=(Join-Path $stage 'error.json') }
    $launchBefore = [IO.File]::ReadAllText((Join-Path $root '.portal-launch.json'))
    Assert (-not (Test-Path -LiteralPath (Join-Path $root 'portal.toml'))) 'config was created beside the exe'
    Assert ((Read-PortalJson (Join-Path $root '.portal-launch.json')).arguments[1] -eq $configPath) 'supervisor lost the central config'
    $configBefore = [IO.File]::ReadAllText($configPath)
    $hashBefore = (Get-FileHash -LiteralPath $exe).Hash
    foreach ($phase in @('checksum', 'verification')) {
        if ($phase -eq 'verification') { $request.sha256 = (Get-FileHash -LiteralPath $stagedExe).Hash }
        Write-PortalJson (Join-Path $stage 'request.json') $request
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + (ConvertTo-PortalArgument (Join-Path $stage 'portal-upgrade-worker.ps1'))
        $info.UseShellExecute = $false; $info.CreateNoWindow = $true
        $info.EnvironmentVariables['HOME'] = $profileRoot
        $info.EnvironmentVariables['USERPROFILE'] = $profileRoot
        $process = [Diagnostics.Process]::Start($info)
        try { Assert ($process.WaitForExit(90000) -and $process.ExitCode -ne 0) 'injected upgrade failure did not finish' }
        finally { if (-not $process.HasExited) { $process.Kill(); [void]$process.WaitForExit(5000) }; $process.Dispose() }
        if ($phase -eq 'checksum') {
            Assert ((Test-PortalReady $root) -and (Runtime).pid -eq $initial.pid) 'invalid download interrupted the old Portal'
            foreach ($record in @(Kit-Records $initial.pid)) { Assert ([bool](Get-Process -Id $record.pid -ErrorAction SilentlyContinue)) 'invalid download interrupted a running Kit' }
            Write-Output 'PASS: rejected download preserves the original Portal, guardian and Kit trees'
        }
    }
    $status = Read-PortalJson (Join-Path $root '.portal-upgrade-status.json')
    Assert ($status.state -eq 'rolled_back') "post-start failure did not roll back: $($status.message)"
    $rejected = Read-PortalJson (Join-Path $root 'rejected-runtime.json')
    Assert ($rejected.pid -and $rejected.pid -ne $initial.pid) 'fault did not run against the replacement Portal'
    Assert-KitsGone $initial.pid
    Assert-KitsGone $rejected.pid
    Assert ((Test-PortalReady $root $version) -and (Test-SavedSupervisor (Runtime))) 'rollback did not restore readiness and supervision'
    Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $hashBefore) 'rollback changed the previous binary'
    Assert ([IO.File]::ReadAllText((Join-Path $root '.portal-launch.json')) -eq $launchBefore) 'rollback changed launch settings'
    Assert ([IO.File]::ReadAllText($configPath) -eq $configBefore) 'rollback changed config'
    Write-Output 'PASS: failed replacement cleans both Kit generations and restores binary, settings and supervision'

    $initial = Runtime
    Wait-KitTree $initial.pid
    Run-Portal @('stop')
    Assert-KitsGone $initial.pid
    Write-Output 'PASS: stop cleans initialized Kits without affecting independent processes'
    $passed = $true
} finally {
    $taskName = Get-PortalSavedValue $root '.portal-task-name'
    if ($taskName) {
        Assert-PortalTaskOwnership $root $taskName
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    }
    # Failed upgrade fixtures may still have a detached worker. Match its exact
    # per-request script before stopping it, then stop only this installation.
    foreach ($stage in @(Get-ChildItem -LiteralPath (Join-Path $root '.portal-upgrades') -Directory -ErrorAction SilentlyContinue)) {
        $script = Join-Path $stage.FullName 'portal-upgrade-worker.ps1'
        foreach ($item in @(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'powershell.exe' -and (Test-PortalScriptCommand $_.CommandLine $script) })) {
            Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    Stop-PortalCheckoutProcesses $root
    foreach ($record in @(Kit-Records)) {
        $item = Get-CimInstance Win32_Process -Filter "ProcessId=$($record.pid)"
        if ($item -and $item.ExecutablePath -eq $python -and $item.CommandLine.Contains($root)) { Stop-Process -Id $item.ProcessId -Force -ErrorAction SilentlyContinue }
    }
    if ($outsider) { if (-not $outsider.HasExited) { $outsider.Kill(); [void]$outsider.WaitForExit(5000) }; $outsider.Dispose() }
    $resolved = [IO.Path]::GetFullPath($fixtureRoot)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -notlike 'portal-kit-lifecycle-*') { throw 'Unsafe fixture cleanup path' }
    if ($passed) { Remove-Item -LiteralPath $resolved -Recurse -Force }
    else { Write-Output "Failed fixture retained: $resolved" }
}
