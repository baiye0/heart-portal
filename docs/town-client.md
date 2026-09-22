# Town-Client integration

Portal supports both standalone operation and desktop-managed operation from main.
Town-Client pins this repository as a source submodule, builds it on each target
platform, and ships it inside the client release. No separate Portal release is
needed for client updates.

The client supplies `HEART_PORTAL_CLIENT_MANAGED=1`, `HEART_PORTAL_SUPERVISED=1`
and an explicit `--config`. This launch stays in the client-owned installation
instead of moving to the standalone installation. These environment markers are
lifecycle coordination, not an authorization boundary.

The client stops and disables its OS supervisor, verifies the old engine exited,
then replaces the application and starts the bundled engine with the same config.
Portal's standalone upgrade command rejects client-managed launches. Standalone
Portal installation and upgrades otherwise retain upstream behavior.

`HEART_PORTAL_READY_NONCE` and `HEART_PORTAL_READY_FILE` identify the supervised
launch. Both macOS and Windows publish PID/nonce-bound connection telemetry;
readiness remains independent of cloud reachability. The client owns rollback.

## Shared main and desktop behavior

Both launch modes include the same sub-agent runtime.
A standalone launch retains its shell behavior and does not advertise client commands.
When `HEART_PORTAL_CLIENT_FILE` and a connection link register the desktop handler,
`portal_exec` routes commands beginning with `@` to the client, including
`@context [scene_id]` and `@scenes`. Set `shell` explicitly (`default` or, on Windows,
`powershell`) to execute literal shell syntax beginning with `@` instead.
Client request failures never fall back to shell execution.

Scene IDs are read from `params._meta`, `params.meta`, then envelope `meta`/`_meta`;
metadata without a string `scene_id` does not prevent fallback to the next source.
The hidden `--exec-enabled` and `--kits-enabled` flags override configuration for
that process only. With no override, the saved configuration remains authoritative.
Client-managed upgrade attempts are rejected before installation migration.
