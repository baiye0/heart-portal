# macOS installation, supervision and signed upgrades

The release remains a standalone executable. Original config, `--config
--connect --name`, `start.sh`, `--upgrade` and `upgrade` entries remain usable.
No new installer shell script or DMG is required.

## Automatic supervision

Normal startup retains the foreground Portal PID and attaches an embedded
Python 3.9+ session supervisor. Its detached process inherits the original
Terminal/app responsibility used by TCC. It restarts the same signed path,
arguments, working directory and environment. Credentials stay in memory, out
of supervisor argv and files. Support scripts share one private directory;
start/status/stop requests are passed through stdin. SIGTERM/SIGKILL and `portal_restart` are
restartable; foreground Ctrl+C or `heart-portal stop` stops supervision.
`heart-portal status` reports Portal and supervisor PIDs. A kernel lock prevents
duplicate supervisors; failed config validation does not attach one.

This covers the current login session. Login startup retains the existing
LaunchAgent management command, run as the logged-in GUI user without sudo:

```sh
python3 scripts/portal-macos.py install --root /installed/folder --name machine-name
python3 scripts/portal-macos.py status --root /installed/folder
python3 scripts/portal-macos.py uninstall --root /installed/folder
```

Install prompts privately for the Loom URL, or reads `PORTAL_CONNECT_LINK` /
`.portal-connection.url`. It starts Portal immediately with KeepAlive. Managed
Portal does not create an extra session supervisor. Config/name/credentials
survive uninstall. Switching a Terminal-attributed installation to launchd may
require authorization; ordinary upgrades do not make that switch automatically.
Python 3.9+ is required for supervision and upgrades; `.portal-python` can record
its absolute path. A fresh Mac may need Python installed.

## Coordinated signed upgrade

```sh
./heart-portal upgrade
./heart-portal upgrade --file /absolute/path/to/new-portal
./heart-portal upgrade --status
```

Before interruption, the candidate must satisfy the trusted Developer ID
Application requirement: Team `7N8XHQWCNN`, identifier `com.aspect.heart-portal`,
Apple generic anchor and the Developer ID leaf OID. The updater checks mutual
designated requirements to record whether the existing signing identity is
preserved. **An old identity difference does not block upgrading**; status and
the acceptance response explain that macOS may require one-time authorization.
Unsigned, tampered or wrong-publisher candidates still fail validation. The
candidate must be newer and match release metadata/digest. No re-signing,
xattr stripping or TCC database editing occurs.

A shared maintenance lock excludes concurrent install/uninstall/upgrade/start.
The session supervisor pauses respawn during replacement and starts the new
binary only on the active worker's request, retaining its TCC responsibility.
An existing LaunchAgent is paused and restarted from the same plist; an
independent worker survives its bootout. Fresh PID/version/nonce readiness and
one stable runtime are checked independently of network/relay availability.

Without a guardian, the old start.sh restarts Portal, or the user starts it
manually as before. The new binary attaches supervision after the transaction
commits. The upgrade CLI returns acceptance through portal_exec before stopping
Portal; acceptance is not completion, so inspect upgrade --status.

Startup failure restores previous bytes atomically. If a worker is killed,
the existing session supervisor detects the interrupted journal after the
maintenance lock is released and reruns the saved worker to roll back. An
existing LaunchAgent installation uses its independent worker's KeepAlive.
There is no separate recovery watcher or new LaunchAgent for a Terminal session.

If an unmanaged legacy worker is killed, or the entire Terminal session/reboot
removes its supervisor, rerun the saved worker **from the original Terminal/app**.
Do not launch the interrupted candidate before recovery:

```sh
# Run in the installation directory, after the interrupted worker has exited.
python3 "$(python3 -c 'import json; print(json.load(open(".portal-upgrade.json"))["stage"] + "/portal-macos-upgrade.py")')"
```

A session whose original supervisor has gone requires manual restart after
rollback. Backups/logs remain under `.portal-upgrades`; completed worker plists
are removed and their loaded jobs have no running process.

## First migration from the real published 0.8.0

The [GitHub v0.8.0 asset](https://github.com/d5z/heart-portal/releases/tag/v0.8.0)
ships with a Developer ID signature. Its unmodified startup code nevertheless
calls unlock_gatekeeper, strips xattrs and re-signs itself ad-hoc. Its updater
also re-signs downloaded files. This was reproduced with the original GitHub
asset, not a locally compiled version substitute.

Use the downloaded new executable for this first migration so the obsolete
updater cannot re-sign the new release. No identity-change opt-in is required:

```sh
/path/to/new/heart-portal-macos-arm64 upgrade \
  --target /installed/heart-portal
```

Status records signature_identity_preserved=false and a permission notice.
The candidate must still have the expected publisher and a valid signature.
**Direct grants tied to the old ad-hoc identity may need one authorization.**
They cannot be guaranteed to survive migration to Developer ID. Subsequent
signed releases keep a compatible identity; all upgrades report compatibility
without rejecting an old identity change. Stable path and TCC responsible-process attribution matter alongside
signing. Kits such as cua-driver need separate signature/permission validation.

## Local zero-install validation

```sh
python3 scripts/tests/build-macos-local-test.py
python3 scripts/tests/macos-published-upgrade-e2e.py \
  --package dist/macos-user-test --fresh \
  --root "$HOME/heart-portal-zero-install-test" \
  --require-permissions screen_recording accessibility input_monitoring
```

The builder downloads GitHub 0.8.0 and verifies GitHub's SHA-256 plus its publisher
signature, then builds/signs 0.8.1 and an artificial 0.8.2 for subsequent-upgrade
testing. Only these two local builds differ by Cargo version. Never publish
0.8.2. The --release-metadata option can reuse previously fetched GitHub metadata
when the API is rate limited; the downloaded asset digest is still verified.

--fresh requires a nonexistent root. The E2E installs the unchanged GitHub
binary, creates config/workspace, starts a loopback relay with no real Being,
observes the old binary's self-re-signing and absence of supervision, migrates
to 0.8.1 without an identity override, starts the same original
command and verifies automatic supervision. It tests crash/controlled restart,
then a normal supervised upgrade to 0.8.2. Config, path, exact signed bytes,
version, single runtime and guardian are checked. Add --legacy-start-script to
exercise a pre-existing start.sh. All test processes are stopped on completion.

0.8.0 has no portal_permissions tool. Its baseline uses a non-prompting child
probe through its real portal_exec and a one-pixel screen-capture test. New
versions measure permissions inside Portal. Reports distinguish this scope and
unverified permissions; false-before/false-after is not permission retention.
A fresh installation directory does not simulate a fresh TCC database. The test
does not reset permissions or claim to prove direct binary grants on other Macs.

New artifacts use Developer ID Application: D5 Inc. (7N8XHQWCNN), stable
identifier, hardened runtime and secure timestamp. Notarization is postponed
as requested. Other Macs may still require Gatekeeper approval; this is not a
notarized distribution acceptance test. The receiving Mac does not need the
developer certificate/private key to verify or run an already signed artifact.

Normal build/sign: `python3 scripts/package-portal-macos.py`. This helper only
builds and signs the raw executable; it does not submit notarization requests.

Apple references:

- [Code signing requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)
- [Code signing in depth](https://developer.apple.com/library/archive/technotes/tn2206/)
- [Responsible processes](https://developer.apple.com/documentation/Security/applying-launch-environment-and-library-constraints)
- [Notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
