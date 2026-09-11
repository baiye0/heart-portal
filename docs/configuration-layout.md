# Configuration layout and migration

Portal uses a stable user directory. On Windows and macOS the downloaded
executable installs and delegates to the runtime here; subsequent launches use
the installed version, including after an upgrade. Linux uses the same config
defaults and retains its existing OS service management.

```text
~/.heart-portal/
├── portal.toml                  # default configuration, optionally connect link
├── profiles/<name>/portal.toml  # optional independent Portal configurations
├── kits/<kit>/                 # default kit code, manifest, .env and local credentials
├── workspace/                  # workspace for a new default installation
├── locks/                      # macOS instance locks
└── runtime/                    # executable, support scripts, logs, locks,
                               # launch settings, readiness and upgrade/rollback files
```

On Windows, `~` consistently means `%USERPROFILE%` (falling back to `HOME`).
On Unix it uses `HOME`, or the account directory when services omit `HOME`.
The downloaded file remains untouched and creates no runtime files beside it.
The installed executable and upgrade backups stay on the same filesystem so
replacement and rollback retain their atomic operation. Explicit
workspaces and kit locations can remain elsewhere; centralizing the config
must not silently change the files a Being can access.

## Stable configuration selection

1. Explicit `--config` / positional config path.
2. The config path in this installation's saved launch, when present.
3. Before installation, an existing legacy `portal.toml` beside the download
   or in the current directory is copied through the checked migration below.
   An installed runtime never selects a config from an unrelated current directory.
4. `~/.heart-portal/portal.toml` for a new installation.

Config and saved launch/identity metadata reads are limited to 1 MiB and regular,
non-symlink files; oversized files fail before parsing.
A missing explicit/saved config or unreadable/invalid config is an error. It
does not fall back to another Portal's configuration. A saved instance is not
silently switched when a central config already exists. Changing only `--name`
or `--connect` retains the saved config path.

First startup creates the default config privately without overwriting an existing
file. `heart-portal config init` performs the same initialization without starting
Portal. Installation scripts reuse this command instead of writing their own config.

Run `heart-portal config path` to inspect selection without printing credentials
or creating files. For an explicit configuration use:

```text
heart-portal --config /absolute/path/portal.toml config path
```

`workspace` and `kits_dir` support absolute paths, `~/` and paths relative to the
config file, independent of the shell/service working directory. For a legacy relative `kits_dir`, an existing directory relative to the
launch working directory is preserved and reported as `legacy-kits-directory`,
even if a config-relative directory also exists. Use an explicit absolute path
to make this unambiguous. Existing default values inside legacy configs remain compatible; the new default
configuration uses its own `./workspace` directory.

Unsupported settings produce startup warnings, also available in
`portal_status.config.warnings`. This includes `[custom_tools].config_path`,
which is not a supported override: custom definitions come from
`<effective workspace>/tools/mcp.toml`, controlled by
`[tools].custom_tools_enabled`. Unknown settings remain ignored for backwards
compatibility; they do not activate another directory. Warnings identify fields
and precedence without printing configuration values. Use the effective paths
from `portal_status` instead of inferring them from the executable directory.

## Migrate an existing config

Preview first:

```text
heart-portal config migrate --from /old/installation/portal.toml --profile desktop
```

Publish the copy:

```text
heart-portal config migrate --from /old/installation/portal.toml --profile desktop --apply
```

The profile is optional. With no profile the destination is the default central
`portal.toml`. Existing different destination contents are never overwritten;
use a different profile when the central config belongs to another instance.

Migration preserves effective workspace and kit paths by recording them as
absolute paths. A relative legacy kits path requires the matching saved
launch working directory, or a migration run from the source config directory;
an ambiguous migration is rejected. Migration retains TOML settings including unknown extension tables, and
imports a saved name/connection link from matching legacy metadata. It refuses
to drop a saved `PORTAL_MCP_TOKEN`: that effective token is preserved privately
in the migrated configuration. It also refuses
to import a launch record for a different config. `connect` is a persistent
TOML setting; `--connect` and `PORTAL_CONNECT_LINK` still take precedence.
When the current executable's saved launch points to the source config, its
installation directory is used to import identity even if the config is external.
To migrate an external config belonging to another installation, add
`--installation /old/installation` to both preview and apply. The saved launch
must match the source; unrelated installation credentials are never imported.
Saved explicit launch environment values continue to take precedence on
supervisor restarts, so reconnect through the normal CLI when changing them.

The destination is published atomically with private file permissions (Unix
0600 / Windows protected DACL). Publication never replaces a file created
concurrently. Identical repeated migrations are idempotent. A failure leaves
the source and any existing destination intact. A crash can leave an unused
temporary file, not a partially published config.

Migration **copies and prepares** configuration. It does not stop Portal,
replace its executable, modify OS autostart registration, move kit credentials,
or activate a different account in a running instance. At the next planned
restart, launch with `--config <returned destination>`. Keep the original config
until the new instance is verified; rollback uses its original `--config` path.
TOML formatting/comments may be normalized in the copy; the source remains exact.

## Move an existing runtime

Stop the old installation with its executable's `stop` command, then launch
the new download directly. An active old supervisor or interrupted upgrade
blocks the move: recover that transaction first. Do not copy live PID records,
locks or upgrade journals. The new installation creates fresh supervision state
and preserves the old files for inspection. Existing workspace/kit paths stay
unchanged; these can be moved separately through explicit configuration.

When the central config already belongs to another instance, migration refuses
to overwrite it. Select a prepared profile with `--config`. One user runtime
is managed at a time; stop it before switching settings. A downloaded older
binary never downgrades the installed runtime. Use `upgrade --file <new binary>`
to update the installed version through its normal verified transaction.

Old releases' in-place upgrade workers cannot migrate their own live supervisor
to another directory. Staged candidates outside the user directory reject their
version preflight before those workers stop the old process; do not rely on an
old worker rolling back a directory relocation. Perform the stopped move before using the new layout's
upgrade path. On macOS, the initial relocation preserves signed bytes, extended
attributes and Terminal/app launch responsibility; macOS may ask for permissions
again because the executable path changed. Later upgrades keep this path stable.

OS login registration remains in the required system location: Windows Task
Scheduler and macOS `~/Library/LaunchAgents`. Third-party kit programs and
explicitly configured workspaces may write to their own locations.

The macOS `portal-macos.py install` entry saves the selected config path in its
LaunchAgent arguments. New installations use the user directory; reinstallation
retains a saved profile. To activate a migrated profile at a planned restart, use
`portal-macos.py install --config /absolute/path/to/migrated/portal.toml`.
Existing LaunchAgents without a config argument retain their legacy config until
reinstalled. Installation rollback restores the prior service registration and
credentials; it never rewrites or deletes the shared user config.
