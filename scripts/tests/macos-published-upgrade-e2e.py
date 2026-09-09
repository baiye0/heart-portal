#!/usr/bin/env python3
"""Unmodified GitHub v0.8.0 first launch -> signed migration -> normal signed upgrade."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('e2e', Path(__file__).with_name('macos-upgrade-e2e.py'))
e2e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e2e)
manager, worker = e2e.manager, e2e.worker
require, wait_for = e2e.require, e2e.wait_for


def probe_legacy(relay):
    # v0.8.0 has no portal_permissions tool. This explicitly measures a child
    # launched by its real portal_exec, not an invented in-process result.
    script = '''import ctypes,json
lib=ctypes.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
names={'screen_recording':'CGPreflightScreenCaptureAccess','accessibility':'AXIsProcessTrusted','input_monitoring':'CGPreflightListenEventAccess'}
result={}
for name,symbol in names.items():
 f=getattr(lib,symbol);f.restype=ctypes.c_bool;result[name]=bool(f())
print(json.dumps(result))
'''
    reply = relay.tool('portal_exec', {'command': shlex.join(['/usr/bin/python3', '-c', script]), 'timeout_secs': 20})
    require(not reply.get('isError'), 'Legacy permission probe failed: ' + json.dumps(reply))
    return json.loads(reply['content'][0]['text'].strip())


def screenshot_probe(relay, root):
    # Exercise actual capture permission with one pixel; do not record or show
    # the user's screen contents as part of a lifecycle test.
    path = root / 'capture-permission-probe.png'
    command = shlex.join(['/usr/sbin/screencapture', '-x', '-R0,0,1,1', str(path)])
    reply = relay.tool('portal_exec', {'command': command, 'timeout_secs': 20})
    result = not reply.get('isError') and path.is_file() and path.read_bytes().startswith(b'\x89PNG')
    path.unlink(missing_ok=True)
    return result


def terminal_state(root):
    value = e2e.read_json(root / '.portal-upgrade-status.json')
    return value if value.get('state') in ('succeeded', 'failed', 'rolled_back') else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, default=REPO / 'dist/macos-user-test')
    parser.add_argument('--root', type=Path, default=Path.home() / '.heart-portal-published-e2e')
    parser.add_argument('--fresh', action='store_true', help='Require an entirely nonexistent installation directory.')
    parser.add_argument('--legacy-start-script', action='store_true', help='Also exercise a pre-existing start.sh; no script is required by the normal fixture.')
    parser.add_argument('--require-permissions', nargs='+', choices=['screen_recording', 'accessibility', 'input_monitoring'])
    args = parser.parse_args()
    package, root = args.package.resolve(), args.root.expanduser().resolve()
    versions = json.loads((package / 'versions.json').read_text())
    filename = versions['binary_name']
    old = package / versions['initial'] / filename
    new = package / versions['first_upgrade'] / filename
    next_version = package / versions['candidate'] / filename
    provenance = json.loads((old.parent / 'github-provenance.json').read_text())
    require(hashlib.sha256(old.read_bytes()).hexdigest() == provenance['sha256'], 'Baseline is not the original GitHub asset.')
    started_empty = not root.exists()
    require(not args.fresh or started_empty, '--fresh requires a nonexistent root; choose a new path instead of deleting an installation.')
    require(not root.exists() or (root / '.portal-published-e2e-owned').is_file(), 'Refusing to modify an unowned test root.')
    root.mkdir(parents=True, exist_ok=True)
    (root / '.portal-published-e2e-owned').touch()
    os.chmod(root, 0o700)
    target = root / 'heart-portal'
    label = manager.label_for(root)
    service = f'gui/{os.getuid()}/{label}'
    plist = Path.home() / 'Library/LaunchAgents' / (label + '.plist')
    report = {'result': 'failed', 'github_baseline': provenance, 'checks': {},
              'installation_started_empty': started_empty, 'root': str(root),
              'fresh_macos_tcc_database': False, 'notarized': False,
              'legacy_start_script': args.legacy_start_script}
    relay = e2e.Relay()
    processes = []
    launch_cwd = root if args.legacy_start_script else root / 'launch cwd 中文'
    launch_cwd.mkdir(exist_ok=True)
    launch_env = dict(os.environ, PORTAL_MIGRATION_TEST='original environment = preserved')
    launch_arguments = ['--config', 'portal.toml' if args.legacy_start_script else '../portal.toml',
                        '--name', 'github-old-user']

    def start():
        with open(root / 'foreground.log', 'ab') as log:
            process = subprocess.Popen([str(target), *launch_arguments, '--connect', link],
                                       cwd=launch_cwd, env=launch_env, stdout=log, stderr=log)
        processes.append(process)
        return process

    try:
        manager.stop_supervisor(root)
        worker.stop(root, service)
        target.unlink(missing_ok=True)
        shutil.copy2(old, target)
        require(worker.verify_signatures(target, new), 'Downloaded baseline should have a compatible Developer ID signature.')
        report['checks']['installed_bytes_match_github_asset'] = target.read_bytes() == old.read_bytes()
        config = root / 'portal.toml'
        config.write_text('workspace = "./workspace"\nbind = "127.0.0.1:0"\nkits_enabled = false\n[cowork]\nenabled = false\n')
        (root / 'workspace').mkdir(exist_ok=True)
        (launch_cwd / 'workspace').mkdir(exist_ok=True)
        config_bytes = config.read_bytes()
        link = f'http://127.0.0.1:{relay.port}/published/?token=local-fixture-only'
        if args.legacy_start_script:
            (root / 'start.sh').write_text('#!/bin/sh\nexec ' + shlex.join([str(target), '--config', str(config), '--connect', link, '--name', 'github-old-user']) + '\n')
        else:
            (root / 'start.sh').unlink(missing_ok=True)
        process = start()
        hello = relay.connect()
        require(not manager.supervisor_state(root), 'Published baseline unexpectedly has an automatic supervisor.')
        require(manager.launchctl('print', service, check=False).returncode != 0, 'Baseline unexpectedly has a LaunchAgent.')
        details = worker.checked('/usr/bin/codesign', '-dv', str(target))
        require('Signature=adhoc' in details, 'Published startup did not match the known self-re-signing behavior; investigate before migrating.')
        report['legacy_after_first_launch'] = {'version': worker.version(target), 'signature': 'adhoc',
                                               'sha256': hashlib.sha256(target.read_bytes()).hexdigest(), 'pid': process.pid,
                                               'supervised': False}
        report['before'] = {'permissions': probe_legacy(relay), 'scope': 'Non-prompting child probe launched through the unmodified GitHub Portal; old Portal lacks an in-process API.',
                            'screen_capture_succeeded': screenshot_probe(relay, root)}
        missing = [name for name in (args.require_permissions or []) if not report['before']['permissions'][name]]
        require(not missing, f'Baseline lacks {missing}. Grant the original Terminal/app these permissions, restart it if macOS requires, then rerun. No TCC grants are reset by this test.')
        accepted = relay.tool('portal_exec', {'command': shlex.join([str(new), 'upgrade', '--target', str(target)]), 'timeout_secs': 360})
        report['checks']['legacy_identity_change_does_not_block_upgrade'] = True
        require('Upgrade accepted' in json.dumps(accepted), 'Migration did not return acceptance through the old relay.')
        require('Signing identity changed' in json.dumps(accepted), 'Missing first-migration permission notice.')
        migrated = wait_for(lambda: terminal_state(root))
        require(migrated['state'] == 'succeeded', f'Published migration failed: {migrated}')
        process.wait(timeout=20)
        require(target.read_bytes() == new.read_bytes(), 'First migration altered the signed candidate.')
        require(worker.verify_signatures(new, target), 'Installed signature differs from the new artifact.')
        require(len(manager.checkout_pids(root)) == 1, 'First migration did not automatically restart Portal.')
        require(relay.connect() == hello, 'First migration changed relay identity.')
        after = relay.tool('portal_permissions')['status']
        require(manager.supervisor_state(root), 'The new release did not automatically attach supervision.')
        if not args.legacy_start_script:
            launch = manager.launch_snapshot(manager.process_identity(after['pid']))
            require(launch['arguments'] == [*launch_arguments, '--connect', link], 'First migration changed argument boundaries or relative config.')
            require(launch['cwd'] == str(launch_cwd), 'First migration changed working directory.')
            require(launch['environment'].get('PORTAL_MIGRATION_TEST') == launch_env['PORTAL_MIGRATION_TEST'],
                    'First migration lost the runtime environment.')
            report['checks']['original_argv_cwd_environment_preserved'] = True
        require(config.read_bytes() == config_bytes, 'First migration changed configuration.')
        report['first_upgrade'] = {'status': migrated, 'permissions': after,
                                   'screen_capture_succeeded': screenshot_probe(relay, root),
                                   'supervisor_attached_automatically': True, 'restarted_without_manual_launch': True}
        # The legacy signature changes, so inherited-grant observations must
        # never be described as a compatible-signature or clean-machine proof.
        inherited = [key for key, granted in report['before']['permissions'].items() if granted]
        report['tcc_retention'] = {'verified_inherited_permissions': inherited,
                                   'unverified_permissions': [key for key in report['before']['permissions'] if key not in inherited],
                                   'direct_binary_legacy_grants_verified': False}
        require(all(after['permissions'][key] for key in inherited), 'A measured inherited permission was lost on this machine.')
        if report['before']['screen_capture_succeeded']:
            require(report['first_upgrade']['screen_capture_succeeded'], 'Real screen capture stopped working after migration.')
        os.kill(after['pid'], signal.SIGKILL)
        require(relay.connect() == hello, 'Automatic crash recovery changed connection identity.')
        recovered = relay.tool('portal_permissions')['status']
        require(recovered['pid'] != after['pid'] and recovered['permissions'] == after['permissions'], 'Crash recovery changed grants.')
        relay.tool('portal_restart')
        require(relay.connect() == hello, 'Controlled restart failed.')
        baseline = relay.tool('portal_permissions')['status']
        accepted = relay.tool('portal_exec', {'command': shlex.join([str(target), 'upgrade', '--file', str(next_version)]), 'timeout_secs': 360})
        require('Upgrade accepted' in json.dumps(accepted), 'Normal supervised upgrade did not acknowledge before disconnect.')
        require(relay.connect() == hello, 'Subsequent upgrade changed connection identity.')
        upgraded = wait_for(lambda: terminal_state(root))
        require(upgraded['state'] == 'succeeded', f'Subsequent upgrade failed: {upgraded}')
        final = relay.tool('portal_permissions')['status']
        require(final['permissions'] == baseline['permissions'], 'Compatible signed upgrade changed permissions.')
        require(target.read_bytes() == next_version.read_bytes(), 'Subsequent upgrade altered signed bytes.')
        require(worker.verify_signatures(new, target), 'Installed signature differs from the new artifact.')
        require(manager.checkout_pids(root) == [final['pid']] and manager.supervisor_state(root), 'Guardian/runtime duplicated or disappeared.')
        require(config.read_bytes() == config_bytes, 'Subsequent upgrade changed config.')
        report['subsequent_upgrade'] = {'status': upgraded, 'permissions': final,
                                        'screen_capture_succeeded': screenshot_probe(relay, root)}
        report['checks']['real_github_install_migration_then_supervised_upgrade'] = True
        report['checks']['crash_and_controlled_restart'] = True
        report['permission_scope'] = 'Observed Terminal/app-inherited grants on this Mac. Ad-hoc-to-Developer-ID first migration cannot guarantee retaining direct binary TCC grants on other Macs.'
        report['result'] = 'passed'
    finally:
        manager.stop_supervisor(root)
        worker.stop(root, service)
        for process in processes:
            process.wait(timeout=20)
        relay.close()
        plist.unlink(missing_ok=True)
        (root / 'verification-report.json').write_text(json.dumps(report, indent=2))
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Published E2E failed: {error}', file=sys.stderr)
        sys.exit(1)
