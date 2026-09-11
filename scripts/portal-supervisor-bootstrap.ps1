# Stable lifecycle-v1 launcher. Updated supervisor code runs in a separate
# process, so replacing/restarting it never removes the recovery owner.
param([Parameter(Mandatory=$true)][string]$Root, [string]$PortalName = '', [int]$RestartDelaySeconds = 5)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'portal-lifecycle.ps1')
$Root = (Resolve-Path -LiteralPath $Root).Path
while ($true) {
    $gate = $null
    $child = $null
    try {
        try { $gate = [IO.File]::Open((Join-Path $Root '.portal-lifecycle.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
        catch [IO.IOException] {
            if (($_.Exception.HResult -band 0xffff) -notin @(32, 33)) { throw }
            Start-Sleep -Milliseconds 250
            continue
        }
        $journalPath = Join-Path $Root '.portal-upgrade.json'
        $recovery = $null
        if (Test-Path -LiteralPath $journalPath) {
            $owner = $null
            try { $owner = [IO.File]::Open((Join-Path $Root '.portal-upgrade.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
            catch [IO.IOException] { if (($_.Exception.HResult -band 0xffff) -notin @(32, 33)) { throw } }
            if ($owner) {
                try {
                    $journal = (Read-PortalText $journalPath) | ConvertFrom-Json
                    if ($journal.recovery_script) {
                        $recovery = [IO.Path]::GetFullPath($journal.recovery_script)
                        $stageRoot = [IO.Path]::GetFullPath((Join-Path $Root '.portal-upgrades')) + '\'
                        if (-not $recovery.StartsWith($stageRoot, [StringComparison]::OrdinalIgnoreCase) -or
                            [IO.Path]::GetFileName($recovery) -ne 'portal-upgrade-worker.ps1') { throw 'Invalid recovery worker path.' }
                    }
                } finally { $owner.Dispose() }
            }
        }
        $info = [Diagnostics.ProcessStartInfo]::new()
        $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $info.UseShellExecute = $false
        $info.CreateNoWindow = $true
        $info.WorkingDirectory = $Root
        if ($recovery) {
            $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}" -Recover' -f $recovery
            $gate.Dispose(); $gate = $null
        } else {
            $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}" -Root "{1}" -PortalName "{2}" -RestartDelaySeconds {3}' -f (Join-Path $Root 'scripts\portal-supervisor.ps1'), $Root, $PortalName, $RestartDelaySeconds
            $info.EnvironmentVariables['HEART_PORTAL_BOOTSTRAP_PID'] = [string]$PID
        }
        $child = [Diagnostics.Process]::Start($info)
        if ($gate) { $gate.Dispose(); $gate = $null }
        $child.WaitForExit()
        # A duplicate supervisor intentionally exits 73. Do not spin a second
        # launcher forever when a user starts it manually beside the task.
        if (-not $recovery -and $child.ExitCode -eq 73) { exit 0 }
    } catch { Write-Warning "Portal bootstrap: $($_.Exception.Message)" }
    finally {
        if ($gate) { $gate.Dispose() }
        if ($child) { $child.Dispose() }
    }
    Start-Sleep -Seconds 1
}
