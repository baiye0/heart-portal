param([string]$Binary = (Join-Path $PSScriptRoot '..\..\dist\heart-portal-windows-x86_64.exe'))
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$tempBase = [IO.Path]::GetTempPath()
$root = Join-Path $tempBase ("portal guidance test ' 测-" + [guid]::NewGuid().ToString('N'))
$app = Join-Path $root 'app'
$configDir = Join-Path $root "configuration ' 测"
$workspace = Join-Path $root 'original-workspace'
foreach ($directory in @($app,$configDir,$workspace)) { [void][IO.Directory]::CreateDirectory($directory) }
$exe = Join-Path $app 'heart-portal.exe'
$config = Join-Path $configDir 'portal.toml'
$relay = $null; $reader = $null
$utf8 = [Text.UTF8Encoding]::new($false)
$script:pendingLine = $null
function Assert([bool]$Value, [string]$Message) { if (-not $Value) { throw "Assertion failed: $Message" } }
function Wait-Until([scriptblock]$Condition, [string]$Message) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt 15) { if (& $Condition) { return }; Start-Sleep -Milliseconds 100 }
    throw "Timed out: $Message"
}
function Wait-Console([string]$Expected) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt 15) {
        if (-not $script:pendingLine) { $script:pendingLine = $reader.StandardOutput.ReadLineAsync() }
        if ($script:pendingLine.Wait(200)) {
            $line = $script:pendingLine.Result; $script:pendingLine = $null
            if ($null -eq $line) { throw "Reader exited: $($reader.StandardError.ReadToEnd())" }
            if ($line.Contains($Expected)) { return }
        }
    }
    throw "Console did not show '$Expected'."
}
function New-FixtureProcess([string]$Program, [string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName=$Program; $info.Arguments=(@($Arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
    $info.WorkingDirectory=$root; $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    $info.StandardOutputEncoding=$utf8; $info.StandardErrorEncoding=$utf8
    $info.EnvironmentVariables['PORTAL_CONNECT_LINK']=''
    $info.EnvironmentVariables['HOME']=Join-Path $root 'profile'
    $info.EnvironmentVariables['USERPROFILE']=Join-Path $root 'profile'
    $info.EnvironmentVariables['HEART_PORTAL_CONSOLE_ROOT']=$app
    $info.EnvironmentVariables['PORTAL_GUIDANCE_TEST_LINK']=[string]$script:fixtureLink
    foreach ($name in @('HEART_PORTAL_SUPERVISED','HEART_PORTAL_READY_FILE','HEART_PORTAL_READY_NONCE')) { $info.EnvironmentVariables.Remove($name) }
    return [Diagnostics.Process]::Start($info)
}
function Run-Fixture([string]$Program, [string[]]$Arguments) {
    $process = New-FixtureProcess $Program $Arguments
    try {
        $output=$process.StandardOutput.ReadToEndAsync(); $errors=$process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(90000)) { $process.Kill(); throw 'CLI timed out.' }
        Assert ($output.Wait(2000) -and $errors.Wait(2000)) 'CLI output pipes close'
        Assert ($process.ExitCode -eq 0) "CLI succeeds: $($errors.Result)"
        return $output.Result
    } finally { $process.Dispose() }
}
try {
    Copy-Item -LiteralPath $Binary -Destination $exe
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0)
    $listener.Start(); $localPort=$listener.LocalEndpoint.Port; $listener.Stop()
    $configText = 'name="name-from-config"' + "`n" + 'workspace="' + $workspace.Replace('\','/') + '"' + "`n" + "bind='127.0.0.1:$localPort'`nkits_enabled=false`n"
    [IO.File]::WriteAllText($config,$configText,$utf8)
    [IO.File]::WriteAllText((Join-Path $workspace 'keep-me.txt'),'original workspace',$utf8)
    $initial = Run-Fixture $exe @('--config',$config,'--name','name-from-cli')
    $lines = $initial -split "`r?`n"
    $prompt = @($lines | Where-Object { $_.StartsWith('$beingLink = Read-Host ') })[0]
    $stop = @($lines | Where-Object { $_.StartsWith('& ') -and $_.EndsWith(' stop') })[0]
    $connect = @($lines | Where-Object { $_.StartsWith('& ') -and $_.Contains(' --connect $beingLink') })[0]
    Assert ($prompt -and $stop -and $connect) 'startup prints executable PowerShell guidance'
    $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $reader = New-FixtureProcess $powershell @('-NoProfile','-NonInteractive','-Command',([IO.File]::ReadAllText((Join-Path $repo 'scripts\portal-console.ps1'))))
    Wait-Console 'Being 连接状态：未配置 Being'
    $relay = New-FixtureProcess (Get-Command python -ErrorAction Stop).Source @((Join-Path $repo 'scripts\tests\relay-fixture.py'),$root)
    $relayOutput=$relay.StandardOutput.ReadToEndAsync(); $relayErrors=$relay.StandardError.ReadToEndAsync()
    Wait-Until { Test-Path -LiteralPath (Join-Path $root 'relay-ready.json') } 'local relay ready'
    $relayInfo=Read-PortalJson (Join-Path $root 'relay-ready.json')
    $fixtureToken = 'fixture-token-$cash;quote'''
    $script:fixtureLink = "http://127.0.0.1:$($relayInfo.port)/fixture-being/?token=$fixtureToken&mode=test"
    # Execute the exact printed commands. Only Read-Host is supplied fixture input.
    $scriptText = 'function Read-Host { param($Prompt); return $env:PORTAL_GUIDANCE_TEST_LINK }' + "`n" + $prompt + "`n" + $stop + "`n" + $connect + "`nexit `$LASTEXITCODE"
    [void](Run-Fixture $powershell @('-NoProfile','-NonInteractive','-Command',$scriptText))
    $launch = Read-PortalJson (Join-Path $app '.portal-launch.json')
    Assert ($launch.arguments[1] -eq $config -and $launch.name -eq 'name-from-cli') 'printed commands preserve external config and CLI name'
    Assert ($launch.environment.PORTAL_CONNECT_LINK -eq $script:fixtureLink) 'full link including shell metacharacters reaches Portal unchanged'
    Assert ([IO.File]::ReadAllText($config) -eq $configText -and [IO.File]::ReadAllText((Join-Path $workspace 'keep-me.txt')) -eq 'original workspace') 'configuration and old workspace are preserved'
    $handshake=Read-PortalJson (Join-Path $root 'relay-handshake.json')
    Assert ($handshake.being_id -eq 'fixture-being' -and $handshake.portal_name -eq 'name-from-cli' -and $handshake.token_matches) 'relay receives the intended Being, name and token'
    $status=Read-PortalJson (Join-Path $app '.portal-connection-status.json')
    $runtime=Read-PortalJson (Join-Path $app '.portal-runtime.json')
    Assert ($status.state -eq 'connected' -and $status.pid -eq $runtime.pid -and $status.nonce -eq $runtime.nonce) 'connected status belongs to the live runtime'
    Wait-Console 'Being 连接状态：已连接 Being'
    $connectedRuntime = $runtime
    $readyBefore = [IO.File]::ReadAllText((Join-Path $app '.portal-ready.json'))
    $launchBefore = [IO.File]::ReadAllText((Join-Path $app '.portal-launch.json'))
    # The fixture accepts just one relay session. Keeping it alive through
    # adoption proves that recovery does not disconnect a working Being.
    foreach ($failure in @('core','both')) {
        $runtime = Read-PortalJson (Join-Path $app '.portal-runtime.json')
        $gate = Open-PortalLock $app '.portal-lifecycle.lock' 15
        Assert ($null -ne $gate) 'fixture owns the lifecycle gate during fault injection'
        try {
            $targets = if ($failure -eq 'both') { @($runtime.bootstrap_pid, $runtime.supervisor_pid) } else { @($runtime.supervisor_pid) }
            foreach ($target in $targets) {
                $guardian = Get-Process -Id $target -ErrorAction Stop
                try { $guardian.Kill(); Assert ($guardian.WaitForExit(5000)) 'fixture guardian exits' }
                finally { $guardian.Dispose() }
            }
            Assert (Test-PortalReady $app) 'Portal remains ready with its guardian gone'
        } finally { $gate.Dispose() }
        if ($failure -eq 'both') { [void](Run-Fixture $exe @()) }
        Wait-Until {
            $current = Read-PortalJson (Join-Path $app '.portal-runtime.json')
            (Test-SavedSupervisor $current) -and $current.supervisor_pid -ne $runtime.supervisor_pid
        } 'replacement guardian takes ownership'
        $current = Read-PortalJson (Join-Path $app '.portal-runtime.json')
        Assert ($current.pid -eq $connectedRuntime.pid -and $current.started -eq $connectedRuntime.started -and $current.nonce -eq $connectedRuntime.nonce) 'recovery retains the original Portal'
        Assert ([IO.File]::ReadAllText((Join-Path $app '.portal-ready.json')) -eq $readyBefore) 'recovery retains readiness'
        Assert ([IO.File]::ReadAllText((Join-Path $app '.portal-launch.json')) -eq $launchBefore) 'recovery retains config/name/link/workspace settings'
        Assert (-not $relay.HasExited) 'original relay session stays open'
        [IO.File]::WriteAllText((Join-Path $root 'relay-probe.txt'),$failure,$utf8)
        Wait-Until {
            $pong = Join-Path $root 'relay-pong.txt'
            (Test-Path -LiteralPath $pong) -and [IO.File]::ReadAllText($pong) -eq $failure
        } 'the original WebSocket still answers ping after adoption'
        $instances = @(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -eq $exe })
        Assert ($instances.Count -eq 1 -and $instances[0].ProcessId -eq $connectedRuntime.pid) 'only the original Portal is running'
    }
    $logLength = (Get-Item -LiteralPath (Join-Path $app 'portal-runtime.log')).Length
    $relay.Kill(); [void]$relay.WaitForExit(5000)
    Wait-Console 'Being 连接状态：Being 未连接，等待自动重试'
    Wait-Until { (Get-Item -LiteralPath (Join-Path $app 'portal-runtime.log')).Length -gt $logLength } 'runtime keeps writing logs after guardian adoption'
    [void](Run-Fixture $exe @('stop'))
    Assert (-not (Test-PortalReady $app)) 'stop terminates the adopted runtime'
    Assert (-not (Test-SavedSupervisor (Read-PortalJson (Join-Path $app '.portal-runtime.json')))) 'stop terminates the replacement guardian'
    Write-Output 'PASS: printed connection commands preserve settings; guardian failures retain the same connected Portal; logs, disconnect status and stop still work'
} finally {
    foreach ($process in @($reader,$relay)) { if ($process) { if (-not $process.HasExited) { $process.Kill(); [void]$process.WaitForExit(5000) }; $process.Dispose() } }
    $taskName=Get-PortalSavedValue $app '.portal-task-name'
    if ($taskName) { Assert-PortalTaskOwnership $app $taskName; Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue }
    Stop-PortalCheckoutProcesses $app
    $resolved=[IO.Path]::GetFullPath($root)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($tempBase),[StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -notlike 'portal guidance test *') { throw 'Unsafe guidance fixture cleanup path.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
