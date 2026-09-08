# Windows upgrade contract

`heart-portal.exe` is a directly runnable Portal, not an installer. Its existing
runtime arguments remain available. Do not overwrite the live exe with a build.

## First run with only one exe

Copy `heart-portal-windows-x86_64.exe` into a writable folder on Windows 10/11.
Double-clicking it automatically creates the default config and matching
supervisor scripts and starts a current-user logon task. The command returns
after Portal is locally ready and supervised. Repeated or concurrent launches
reuse the running Portal. No scripts need to be supplied alongside the exe.
The MSVC runtime is linked statically; Windows PowerShell 5.1 is the only script
host needed. Rust, Python and VBScript are not prerequisites for Portal itself.

Without a Loom link, Portal starts local MCP on `127.0.0.1:9100`. Supply
`--connect <link>` to connect to a Being. `.portal-launch.json` saves config
path, name, connection link, selected environment and working directory;
normal relaunches, logon recovery and upgrades reuse them. This file can contain
credentials. Keep it private and keep the generated files with the exe.

`status` reports Portal/guardian state. `stop` stops both supervisor levels and
Portal and disables the logon task; the next normal launch enables it again.
Stop before changing launch arguments. If Windows policy denies task creation,
the exe still starts a hidden guardian for the current session and explicitly
reports that automatic logon recovery is unavailable. A task runs at user login,
not while the user is logged out.

```powershell
# Use the actual downloaded exe name, or rename it to heart-portal.exe.
.\heart-portal.exe --connect "https://relay/<being>/?token=<token>" --name my-machine
.\heart-portal.exe status

# From another terminal, using the same executable:
.\heart-portal.exe upgrade
.\heart-portal.exe upgrade --status
# A pre-downloaded higher-version exe uses the identical replacement/recovery:
.\heart-portal.exe upgrade --file C:\Downloads\new-portal.exe
```

An upgrade command exiting successfully means the independent worker accepted
the update. The CLI must exit before Windows can replace its executable.
Completion is recorded in `.portal-upgrade-status.json` next to the installation
metadata. States include `waiting_for_exit`, `replacing`, `verifying`,
`succeeded`, `rolled_back`, `failed`, and `recovery_required`. Failed/rolled-back
status makes `upgrade --status` return a nonzero exit code.

## Coordination and recovery

1. Download the binary for the release tag returned by GitHub, never a second
   mutable `latest` URL. Verify the GitHub asset digest when available and run
   the candidate's version check before stopping anything.
2. Take an OS-backed exclusive upgrade lock. Installation and uninstallation
   use this lock too. Concurrent requests fail without affecting the owner.
3. Take the shared lifecycle gate before stopping a runtime. Supervisor process
   creation is inside the same gate; there is no check-then-launch gap. The
   normal five-second restart delay resumes when maintenance ends.
4. For supervised installations, export and syntax-check the new exe's matching
   supervisor payload. Back up the exe and managed scripts in one transaction.
   Keep the stable bootstrap alive while restarting the supervisor core.
5. Save a journal, stop this installation's processes and replace the files.
   Process selection uses exact executable/script paths, never a global
   `taskkill /IM`. The updater does not launch a second supervised Portal.
6. Release the lifecycle gate. Commit only after the expected version publishes
   fresh local readiness and stays alive for five seconds, within sixty seconds.
   Verify that the running supervisor matches the installed script as well.
7. On failure, restore the previous exe and supervisor payload, release the
   gate, and verify that the old runtime starts again.

The lock is owned by an open file handle, not by a marker's existence. Windows
releases it when its process exits; do not delete lock files to recover them.
See Microsoft's [FileShare contract](https://learn.microsoft.com/en-us/dotnet/api/system.io.fileshare).
The durable journal remains until commit/rollback. A surviving supervisor or
bootstrap uses it to recover a killed worker. After machine restart, the
existing logon task provides the same recovery entry point.

The stable `portal-supervisor-bootstrap.ps1` is the lifecycle-v1 recovery entry
point, not part of the mutable supervisor payload. Normal releases update
`portal-supervisor.ps1`, `portal-lifecycle.ps1`, and the hidden-launcher script.
Changing the bootstrap protocol itself requires a separate migration; do not
silently replace this recovery owner in a normal upgrade.

For a legacy unsupervised direct launch, launch arguments, selected environment and
working directory are saved in `.portal-direct.json` (it may contain connection
credentials). A normal failure is rolled back by the worker. If that worker is
killed, launching a functioning Portal exe again runs journal recovery. If the
candidate cannot even start, run the journal's saved `recovery_script` with
PowerShell `-NoProfile -ExecutionPolicy Bypass -File <path> -Recover`.
New normal Windows exe launches always bootstrap supervision automatically.

Local readiness checks configuration, workspace/tool initialization and the
local serving path. It does not assert a successful remote relay handshake.
Active Portal tool calls are interrupted when the runtime is stopped. Config,
connection identity, workspaces and kits are not migrated by binary upgrades.

## Build and verification

```powershell
# Builds in target/portal-package, preserving a live target/release executable.
.\scripts\package-portal-windows.ps1
.\scripts\tests\windows-lifecycle.tests.ps1
.\scripts\tests\windows-start-fallback.tests.ps1
.\scripts\tests\windows-package.tests.ps1
.\scripts\tests\windows-package.tests.ps1 -LocalOnly
.\scripts\tests\windows-upgrade-e2e.ps1
```

The output includes a single runnable exe and a ZIP with optional management
scripts. Package tests start with just the real exe in a temporary directory and
an isolated profile. They create/remove their own real logon task and check
concurrent first launches, crash recovery, upgrade, config preservation and
stop/resume. `-LocalOnly` tests the no-link default listener; the other mode uses
a unique invalid relay address. Neither connects to a real Being.

Lifecycle fixtures also test worker termination, startup failure and supervisor
failure/rollback. The fallback fixture simulates denied task registration and
verifies that a detached guardian still starts and recovers crashes without
holding the CLI's output pipes open. The E2E script compiles a temporary source copy with a higher
version and invokes the public `upgrade --file` CLI. It does not change the
source version or publish a release. These clean-directory tests are not a
fresh Windows VM test and do not prove that a future GitHub `latest` asset has
been uploaded correctly. E2E binaries use a separate build directory and cannot
replace the normal package output.
