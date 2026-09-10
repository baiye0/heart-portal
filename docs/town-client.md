# Town-Client integration

This branch follows upstream main and keeps only desktop lifecycle compatibility.
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
