param([switch]$Recover, [int]$ParentProcessId = 0)
# The CLI writes a private request and starts this worker without a console.
# Keep it independent of the old exe so Windows can release that image first.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'portal-lifecycle.ps1')
$request = Read-PortalJson (Join-Path $PSScriptRoot 'request.json')
try {
    if ($Recover) {
        $gate = Open-PortalLock $request.root '.portal-lifecycle.lock' 15
        if (-not $gate) { throw 'Recovery gate is busy.' }
        $journal = Read-PortalJson (Join-Path $request.root '.portal-upgrade.json')
        try {
            if ($ParentProcessId) {
                $parent = Get-Process -Id $ParentProcessId -ErrorAction SilentlyContinue
                if ($parent) {
                    try { if (-not $parent.WaitForExit(30000)) { throw 'Recovery CLI did not exit.' } }
                    finally { $parent.Dispose() }
                }
            }
            Repair-PortalInterruptedUpgrade $request.root | Out-Null
        }
        finally { $gate.Dispose() }
        if ($journal.direct -and -not (Test-Path -LiteralPath (Join-Path $request.root '.portal-upgrade.json'))) {
            Start-PortalDirect $request.root $journal.direct
            Wait-PortalReady $request.root ''
            Set-PortalUpgradeStatus $request.root 'rolled_back' 'Interrupted upgrade recovered; previous Portal is running.' $journal.version
        }
    } else { Invoke-PortalUpgrade $request }
} catch {
    # A per-request error also lets a rejected concurrent CLI report the reason
    # without overwriting the status belonging to the active upgrade.
    Write-PortalJson $request.error @{ message = $_.Exception.Message }
    exit 1
}
