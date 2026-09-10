param(
    [string]$TaskName = '',
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$managed = Join-Path $env:USERPROFILE '.heart-portal\runtime'
if ((Get-PortalSavedValue $managed '.portal-origin') -eq $Root) { $Root = $managed }
$maintenance = Enter-PortalMaintenance $Root
try {
$nameFile = Join-Path $Root '.portal-name'
$taskNameFile = Join-Path $Root '.portal-task-name'
if ([string]::IsNullOrWhiteSpace($TaskName)) {
    if (Test-Path -LiteralPath $taskNameFile) {
        $TaskName = (Get-Content -LiteralPath $taskNameFile -Raw).Trim()
    } elseif (Test-Path -LiteralPath $nameFile) {
        $portalName = (Get-Content -LiteralPath $nameFile -Raw).Trim()
        if (-not [string]::IsNullOrWhiteSpace($portalName)) {
            $TaskName = "HeartPortal-$portalName"
        }
    }
    if ([string]::IsNullOrWhiteSpace($TaskName)) {
        throw 'TaskName was not supplied and no saved Portal task name exists.'
    }
}

Assert-PortalTaskOwnership $Root $TaskName
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue

Stop-PortalCheckoutProcesses $Root
if (Test-Path -LiteralPath (Join-Path $Root '.portal-upgrade.json')) {
    Restore-PortalUpgrade $Root (Read-PortalJson (Join-Path $Root '.portal-upgrade.json'))
}

Write-Output "Removed scheduled task '$TaskName'."
} finally { foreach ($lock in $maintenance) { $lock.Dispose() } }
