param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$PortalName = '',
    [ValidateRange(1, 300)]
    [int]$RestartDelaySeconds = 5
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-lifecycle.ps1')
$exe = Get-PortalExecutable $Root
$supervisorHash = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
$config = Join-Path $Root 'portal.toml'
$linkFile = Join-Path $Root '.portal-connection.url'
Protect-PortalFile $linkFile
$nameFile = Join-Path $Root '.portal-name'
$stdoutLog = Join-Path $Root 'portal-runtime.log'
$stderrLog = Join-Path $Root 'portal-runtime.err.log'

$launch = Read-PortalJson (Join-Path $Root '.portal-launch.json')
if ($launch) {
    if ($launch.protocol -ne 1 -or -not $launch.identity -or -not $launch.arguments -or -not $launch.working_directory) {
        throw 'Invalid saved Portal launch configuration.'
    }
    $supervisorIdentity = [string]$launch.identity
} else {
# Existing manually installed relay supervisors retain their saved layout.
if (-not (Test-Path -LiteralPath $config)) { throw "Portal config not found: $config" }
if (-not (Test-Path -LiteralPath $linkFile)) { throw "Connection file not found: $linkFile" }

$loomLink = (Get-Content -LiteralPath $linkFile -Raw).Trim()
if ([string]::IsNullOrWhiteSpace($loomLink)) { throw "Connection file is empty: $linkFile" }

if ([string]::IsNullOrWhiteSpace($PortalName) -and (Test-Path -LiteralPath $nameFile)) {
    $PortalName = (Get-Content -LiteralPath $nameFile -Raw).Trim()
}
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    throw "Portal name is not configured. Run install-portal-windows.ps1 or pass -PortalName explicitly."
}

if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') { throw 'Invalid PortalName.' }

# Prevent a manually launched supervisor and the scheduled task from racing.
# Key by relay host + Being, not by the token: rotating credentials must not
# permit a second supervisor for the same relay identity.
$loomUri = [Uri]$loomLink
if (-not $loomUri.IsAbsoluteUri -or $loomUri.Scheme -notin @('http', 'https')) {
    throw 'Connection file does not contain a valid Loom URL.'
}
$beingId = $loomUri.AbsolutePath.Trim('/').Split('/')[0]
if ([string]::IsNullOrWhiteSpace($beingId)) { throw 'Connection file has no Being ID.' }
$supervisorIdentity = "$($loomUri.Authority.ToLowerInvariant())/$beingId"
}
$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
    $identityBytes = [System.Text.Encoding]::UTF8.GetBytes($supervisorIdentity)
    $identityHash = [System.BitConverter]::ToString($sha256.ComputeHash($identityBytes)).Replace('-', '')
} finally {
    $sha256.Dispose()
}
$createdNew = $false
$supervisorMutex = [System.Threading.Mutex]::new($true, "Local\heart-portal-supervisor-$identityHash", [ref]$createdNew)
if (-not $createdNew) {
    $supervisorMutex.Dispose()
    Write-Output 'Another Portal supervisor is already running for this relay/Being; exiting.'
    exit 73
}

# Give the child its own file handles, not pipes drained by this supervisor.
# A replacement core cannot adopt the old core's anonymous pipe readers.
# Restrict inheritance to stdin/stdout/stderr; lifecycle locks must never leak
# into Portal. See CreateProcessW / PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
function Start-PortalLoggedProcess($Info) {
    if (-not ('PortalLoggedProcess' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class PortalLoggedProcess {
    [StructLayout(LayoutKind.Sequential)]
    struct StartupInfo {
        public int cb;
        public IntPtr reserved, desktop, title;
        public int x, y, xSize, ySize, xChars, yChars, fill, flags;
        public short show, reservedSize;
        public IntPtr reservedData, input, output, error;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct StartupInfoEx { public StartupInfo startup; public IntPtr attributes; }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo { public IntPtr process, thread; public int pid, tid; }
    [StructLayout(LayoutKind.Sequential)]
    struct SecurityAttributes { public int size; public IntPtr descriptor; public int inherit; }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern SafeFileHandle CreateFileW(string path, uint access, uint share, ref SecurityAttributes attributes,
        uint creation, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returned);
    [DllImport("kernel32.dll")]
    static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string application, StringBuilder command, IntPtr processAttributes,
        IntPtr threadAttributes, bool inherit, uint flags, IntPtr environment, string directory,
        ref StartupInfoEx startup, out ProcessInfo process);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    static SafeFileHandle OpenInput() {
        var attributes = new SecurityAttributes();
        attributes.size = Marshal.SizeOf(typeof(SecurityAttributes));
        attributes.inherit = 1;
        var handle = CreateFileW("NUL", 0x80000000, 3, ref attributes, 3, 0, IntPtr.Zero);
        if (handle.IsInvalid) {
            int error = Marshal.GetLastWin32Error(); handle.Dispose(); throw new Win32Exception(error);
        }
        return handle;
    }

    public static Process Start(ProcessStartInfo info, string stdout, string stderr) {
        var share = FileShare.ReadWrite | FileShare.Delete | FileShare.Inheritable;
        using (var input = OpenInput())
        using (var output = new FileStream(stdout, FileMode.Create, FileAccess.Write, share))
        using (var error = new FileStream(stderr, FileMode.Create, FileAccess.Write, share)) {
            var startup = new StartupInfoEx();
            startup.startup.cb = Marshal.SizeOf(typeof(StartupInfoEx));
            startup.startup.flags = 0x100; // STARTF_USESTDHANDLES
            startup.startup.input = input.DangerousGetHandle();
            startup.startup.output = output.SafeFileHandle.DangerousGetHandle();
            startup.startup.error = error.SafeFileHandle.DangerousGetHandle();
            IntPtr size = IntPtr.Zero, handles = IntPtr.Zero, environment = IntPtr.Zero;
            bool initialized = false;
            var processInfo = new ProcessInfo();
            try {
                InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
                startup.attributes = Marshal.AllocHGlobal(size);
                if (!InitializeProcThreadAttributeList(startup.attributes, 1, 0, ref size))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                initialized = true;
                handles = Marshal.AllocHGlobal(3 * IntPtr.Size);
                Marshal.Copy(new[] { startup.startup.input, startup.startup.output, startup.startup.error }, 0, handles, 3);
                if (!UpdateProcThreadAttribute(startup.attributes, 0, new IntPtr(0x20002), handles,
                    new IntPtr(3 * IntPtr.Size), IntPtr.Zero, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                var keys = new string[info.EnvironmentVariables.Count];
                info.EnvironmentVariables.Keys.CopyTo(keys, 0);
                Array.Sort(keys, StringComparer.OrdinalIgnoreCase);
                var block = new StringBuilder();
                foreach (var key in keys) block.Append(key).Append('=').Append(info.EnvironmentVariables[key]).Append('\0');
                block.Append('\0');
                environment = Marshal.StringToHGlobalUni(block.ToString());
                var command = new StringBuilder("\"" + info.FileName + "\" " + info.Arguments);
                // CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT
                if (!CreateProcessW(info.FileName, command, IntPtr.Zero, IntPtr.Zero, true, 0x08080400,
                    environment, info.WorkingDirectory, ref startup, out processInfo))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                var process = Process.GetProcessById(processInfo.pid);
                try { var handle = process.Handle; return process; }
                catch { process.Dispose(); throw; }
            } finally {
                if (processInfo.thread != IntPtr.Zero) CloseHandle(processInfo.thread);
                if (processInfo.process != IntPtr.Zero) CloseHandle(processInfo.process);
                if (initialized) DeleteProcThreadAttributeList(startup.attributes);
                if (startup.attributes != IntPtr.Zero) Marshal.FreeHGlobal(startup.attributes);
                if (handles != IntPtr.Zero) Marshal.FreeHGlobal(handles);
                if (environment != IntPtr.Zero) Marshal.FreeHGlobal(environment);
            }
        }
    }
}
'@
    }
    return [PortalLoggedProcess]::Start($Info, $stdoutLog, $stderrLog)
}

try {
    while ($true) {
        $process = $null
        $keepRuntime = $false
        $launchGate = $null
        try {
            # The same exclusive gate covers BOTH checking maintenance and
            # creating the child, so an updater cannot race a checked launch.
            $launchGate = Open-PortalLock $Root '.portal-lifecycle.lock'
            if (-not $launchGate) { Start-Sleep -Milliseconds 250; continue }
            if (Repair-PortalInterruptedUpgrade $Root) { exit 75 }
            if (-not (Test-Path -LiteralPath $exe)) { throw "Portal binary not found: $exe" }
            $runtime = Read-PortalJson (Join-Path $Root '.portal-runtime.json')
            $process = Get-PortalRecordedProcess $Root $runtime
            if ($process) {
                # Ownership changes; PID, nonce, readiness and the live Being
                # session do not. Never erase a live record to try another exe.
                $keepRuntime = $true
                if ($runtime.identity -and $runtime.identity -ne $supervisorIdentity) {
                    throw 'Recorded Portal identity differs from the saved launch; stop it before changing settings.'
                }
                $nonce = $runtime.nonce
            } else {
                # CreateNoWindow isolates Portal from the supervisor's console.
                # A Ctrl+C/taskkill directed at Portal must never terminate the
                # supervisor that is responsible for bringing it back.
                $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
                $startInfo.FileName = $exe
                $startInfo.Arguments = "--config `"$config`" --name `"$PortalName`""
                $startInfo.WorkingDirectory = $Root
                if ($launch) {
                    $startInfo.Arguments = (@($launch.arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
                    $startInfo.WorkingDirectory = $launch.working_directory
                    foreach ($entry in $launch.environment.PSObject.Properties) { $startInfo.EnvironmentVariables[$entry.Name] = [string]$entry.Value }
                }
                $startInfo.UseShellExecute = $false
                $startInfo.CreateNoWindow = $true
                # portal_restart is safe only when an external supervisor is
                # guaranteed to relaunch this process. Keep the credential out of
                # the child command line as well.
                $startInfo.EnvironmentVariables['HEART_PORTAL_SUPERVISED'] = '1'
                if (-not $launch) { $startInfo.EnvironmentVariables['PORTAL_CONNECT_LINK'] = $loomLink }
                $nonce = [guid]::NewGuid().ToString('N')
                $startInfo.EnvironmentVariables['HEART_PORTAL_READY_FILE'] = Join-Path $Root '.portal-ready.json'
                $startInfo.EnvironmentVariables['HEART_PORTAL_READY_NONCE'] = $nonce
                [IO.File]::Delete((Join-Path $Root '.portal-ready.json'))

                $process = Start-PortalLoggedProcess $startInfo
            }
            $self = Get-Process -Id $PID
            try {
                if (-not $process.HasExited) { Write-PortalJson (Join-Path $Root '.portal-runtime.json') @{
                    protocol = 1; pid = $process.Id; nonce = $nonce
                    started = $process.StartTime.ToUniversalTime().Ticks
                    supervisor_pid = $PID; supervisor_started = $self.StartTime.ToUniversalTime().Ticks
                    bootstrap_pid = $env:HEART_PORTAL_BOOTSTRAP_PID
                    supervisor_hash = $supervisorHash
                    identity = $supervisorIdentity
                } }
            } finally { $self.Dispose() }
            $keepRuntime = $true
            $launchGate.Dispose(); $launchGate = $null
            Write-Host "Portal supervised (PID $($process.Id)); waiting for exit..."
            while (-not $process.WaitForExit(1000)) {
                if (Test-Path -LiteralPath (Join-Path $Root '.portal-upgrade.json')) {
                    $launchGate = Open-PortalLock $Root '.portal-lifecycle.lock'
                    if ($launchGate) {
                        try { if (Repair-PortalInterruptedUpgrade $Root) { exit 75 } }
                        finally { $launchGate.Dispose(); $launchGate = $null }
                    }
                }
            }
            Write-Warning "Portal exited with code $($process.ExitCode); restarting in $RestartDelaySeconds seconds"
        } catch {
            Write-Warning "Portal supervisor error: $($_.Exception.Message); retrying in $RestartDelaySeconds seconds"
        } finally {
            if ($launchGate) { $launchGate.Dispose() }
            if ($process -and -not $keepRuntime) {
                # Before publication a failed launch cannot be recovered from
                # its record. A published/adopted runtime remains available to
                # the next core even if supervision itself fails.
                try {
                    if (-not $process.HasExited) {
                        $process.Kill()
                        $process.WaitForExit()
                    }
                } catch { Write-Warning "Portal cleanup: $($_.Exception.Message)" }
            }
            if ($process) { $process.Dispose() }
        }
        Start-Sleep -Seconds $RestartDelaySeconds
    }
} finally {
    $supervisorMutex.ReleaseMutex()
    $supervisorMutex.Dispose()
}
