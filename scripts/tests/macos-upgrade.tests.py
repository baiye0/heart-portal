#!/usr/bin/env python3
"""macOS upgrade regressions: signing rejection + isolated real launchd recovery."""
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('upgrade', REPO / 'scripts/portal-macos-upgrade.py')
upgrade = importlib.util.module_from_spec(spec)
spec.loader.exec_module(upgrade)
manager = upgrade.manager


def wait_for(predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.1)
    raise AssertionError('Timed out waiting for upgrade state')


def read(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


class UnitTests(unittest.TestCase):
    def test_lock_excludes_management_and_releases_on_exception(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with manager.maintenance_lock(root):
                with self.assertRaisesRegex(RuntimeError, 'in progress'):
                    with manager.maintenance_lock(root):
                        self.fail('Second owner acquired lock')
            with manager.maintenance_lock(root):
                pass

    def test_mutual_requirements_are_checked(self):
        calls = []
        def checked(*args):
            calls.append(args)
            if '-d' in args:
                return 'designated => ' + ('old-requirement' if args[-1] == 'old' else 'new-requirement')
            return ''
        with patch.object(upgrade, 'checked', side_effect=checked):
            upgrade.verify_signatures(Path('old'), Path('new'))
        self.assertIn(('/usr/bin/codesign', '--verify', '--strict', '-R', '=old-requirement', 'new'), calls)
        self.assertIn(('/usr/bin/codesign', '--verify', '--strict', '-R', '=new-requirement', 'old'), calls)
        self.assertFalse(any('--force' in c or '-s' in c or 'xattr' in c[0] for c in calls))

    def test_identity_changes_do_not_block_but_untrusted_candidates_do(self):
        def checked(*args):
            if args[-1] == 'old' and '--verify' in args:
                raise RuntimeError('legacy identity')
            return 'designated => new-requirement' if '-d' in args else ''
        with patch.object(upgrade, 'checked', side_effect=checked):
            self.assertFalse(upgrade.verify_signatures(Path('old'), Path('new')))
        with patch.object(upgrade, 'checked', side_effect=RuntimeError('untrusted candidate')):
            with self.assertRaisesRegex(RuntimeError, 'untrusted candidate'):
                upgrade.verify_signatures(Path('old'), Path('new'))


@unittest.skipUnless(sys.platform == 'darwin', 'requires macOS')
class LaunchdTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='portal upgrade 中文 ')
        profile = Path(self.temp.name).resolve()
        home_patch = patch.dict(os.environ, HOME=str(profile))
        home_patch.start()
        self.addCleanup(home_patch.stop)
        self.root = profile / '.heart-portal/runtime'
        self.target = self.root / 'target/release/heart-portal'
        self.target.parent.mkdir(parents=True)
        (self.root / 'scripts').mkdir()
        shutil.copy2(REPO / 'scripts/portal-launchagent.sh', self.root / 'scripts')
        shutil.copy2(REPO / 'scripts/portal-macos.py', self.root / 'scripts')
        (self.root / '.portal-name').write_text('original-name')
        (self.root / '.portal-connection.url').write_text('http://127.0.0.1:9/fixture/?token=test')
        (self.root / 'portal.toml').write_text('workspace = "./workspace"\nkits_enabled = false\nbind = "127.0.0.1:0"\n')
        self.label = manager.label_for(self.root)
        self.domain = f'gui/{os.getuid()}'
        self.service = self.domain + '/' + self.label
        self.plist = Path.home() / 'Library/LaunchAgents' / f'{self.label}.plist'
        self.jobs = []
        self.build(self.target, '0.8.0')
        manager.private_write(self.plist, plistlib.dumps(manager.definition(self.root, self.label)))
        manager.launchctl('bootstrap', self.domain, str(self.plist))
        wait_for(lambda: manager.checkout_pids(self.root))

    def tearDown(self):
        for label, plist in self.jobs:
            manager.launchctl('bootout', self.domain + '/' + label, check=False)
            plist.unlink(missing_ok=True)
        manager.launchctl('bootout', self.service, check=False)
        manager.stop_supervisor(self.root)
        manager.stop_checkout(self.root)
        self.plist.unlink(missing_ok=True)
        self.temp.cleanup()

    def build(self, path, version, broken=False):
        source = self.root / 'fixture.c'
        source.write_text(r'''
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--version")) { puts("heart-portal VERSION"); return 0; }
    if (BROKEN) return 7;
    FILE *f = fopen(".portal-launch-nonce", "r");
    if (f) {
        char nonce[128] = {0}; fgets(nonce, sizeof(nonce), f); fclose(f);
        f = fopen(".portal-ready.json.tmp", "w");
        fprintf(f, "{\"pid\":%d,\"version\":\"VERSION\",\"nonce\":\"%s\"}", getpid(), nonce);
        fclose(f); rename(".portal-ready.json.tmp", ".portal-ready.json");
    }
    for (;;) pause();
}
'''.replace('VERSION', version).replace('BROKEN', '1' if broken else '0'))
        subprocess.run(['/usr/bin/clang', str(source), '-o', str(path)], check=True, capture_output=True)

    def start_upgrade(self, name, broken=False, reject=False, parent=None):
        stage = self.root / '.portal-upgrades' / name
        stage.mkdir(parents=True)
        self.build(stage / 'candidate', '0.8.1', broken)
        for filename in ('portal-macos.py', 'portal-macos-upgrade.py', 'portal-macos-supervisor.py'):
            shutil.copy2(REPO / 'scripts' / filename, stage)
        # Integration isolates lifecycle from Apple credentials/network. The
        # production worker has no bypass flag; only this fixture loader mocks
        # preflight. Separate tests exercise real codesign rejection below.
        wrapper = stage / 'fixture-worker.py'
        wrapper.write_text('''import importlib.util, pathlib, sys
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('worker', pathlib.Path(__file__).with_name('portal-macos-upgrade.py'))
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)
''' + ('' if reject else 'worker.verify_signatures = lambda *args: True\n') + 'sys.exit(worker.main())\n')
        label = self.label + '.upgrade.' + name
        plist = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
        self.jobs.append((label, plist))
        upgrade.write_json(stage / 'request.json', {'root': str(self.root), 'version': '0.8.1', 'parent_pid': parent.pid if parent else 0, 'worker_plist': str(plist),
            **({'parent_executable': str(manager.executable_path(parent.pid))} if parent else {})})
        manager.private_write(plist, plistlib.dumps({'Label': label,
            'ProgramArguments': [sys.executable, str(wrapper)], 'RunAtLoad': True,
            'KeepAlive': {'SuccessfulExit': False}, 'ThrottleInterval': 2,
            'AbandonProcessGroup': True,
            'StandardOutPath': str(stage / 'worker.log'), 'StandardErrorPath': str(stage / 'worker.log')}))
        manager.launchctl('bootstrap', self.domain, str(plist))
        return stage, label

    @unittest.skipUnless(os.environ.get('PORTAL_TEST_BINARY'), 'set PORTAL_TEST_BINARY to test manual recovery')
    def test_manual_restore_waits_for_installer_to_release_lock(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        replacement = self.target.with_name('replacement')
        shutil.copy2(os.environ['PORTAL_TEST_BINARY'], replacement)
        os.replace(replacement, self.target)
        with manager.maintenance_lock(self.root):
            recovery = manager.restore_manual(self.root)
            time.sleep(.3)
            self.assertFalse(manager.checkout_pids(self.root))
        wait_for(lambda: manager.checkout_pids(self.root))
        time.sleep(.3)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)
        manager.stop_supervisor(self.root)
        manager.stop_checkout(self.root)
        recovery.wait(timeout=15)

    @unittest.skipUnless(os.environ.get('PORTAL_TEST_BINARY'), 'set PORTAL_TEST_BINARY to test real readiness')
    def test_real_portal_readiness_and_direct_start_gate(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        replacement = self.target.with_name('replacement')
        shutil.copy2(os.environ['PORTAL_TEST_BINARY'], replacement)
        os.replace(replacement, self.target)
        (self.root / '.portal-launch-nonce').write_text('fresh-test-transaction')
        manager.launchctl('bootstrap', self.domain, str(self.plist))
        try:
            ready = wait_for(lambda: read(self.root / '.portal-ready.json'))
        except AssertionError:
            self.fail((self.root / 'portal-runtime.err.log').read_text() + (self.root / 'portal-runtime.log').read_text())
        expected = subprocess.check_output([str(self.target), '--version'], text=True).strip().split()[1]
        self.assertEqual(ready['version'], expected)
        self.assertEqual(ready['nonce'], 'fresh-test-transaction')
        self.assertEqual(manager.checkout_pids(self.root), [ready['pid']])
        env = os.environ.copy()
        env.pop('HEART_PORTAL_SUPERVISED', None)
        with manager.maintenance_lock(self.root):
            result = subprocess.run([str(self.target), '--config', str(self.root / 'portal.toml')],
                                    cwd=self.root, env=env, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('maintenance/upgrade is in progress', result.stderr)
        self.assertEqual(manager.checkout_pids(self.root), [ready['pid']])

    @unittest.skipUnless(os.environ.get('PORTAL_TEST_BINARY'), 'set PORTAL_TEST_BINARY to test public CLI')
    def test_public_cli_reports_signature_rejection_and_status(self):
        replacement = self.target.with_name('replacement')
        shutil.copy2(os.environ['PORTAL_TEST_BINARY'], replacement)
        os.replace(replacement, self.target)
        candidate = self.root / 'downloaded'
        self.build(candidate, '0.8.1')
        before = self.target.read_bytes()
        pids = manager.checkout_pids(self.root)
        try:
            result = subprocess.run([str(self.target), 'upgrade', '--file', str(candidate)],
                                    capture_output=True, text=True, timeout=75)
        finally:
            for stage in (self.root / '.portal-upgrades').iterdir():
                label = 'town.beings.heart-portal.upgrade.' + stage.name
                self.jobs.append((label, Path.home() / 'Library/LaunchAgents' / f'{label}.plist'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Upgrade rejected', result.stderr)
        self.assertEqual(self.target.read_bytes(), before)
        self.assertEqual(manager.checkout_pids(self.root), pids)
        result = subprocess.run([str(self.target), 'upgrade', '--status'], capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)['state'], 'failed')

    def test_existing_management_script_gains_lock_without_reinstall(self):
        (self.root / 'scripts/portal-macos.py').write_text('# old manager without locking')
        plist = self.plist.read_bytes()
        stage, _ = self.start_upgrade('old-manager')
        result = wait_for(lambda: read(stage / 'result.json'))
        self.assertEqual(result['state'], 'succeeded')
        self.assertIn('LIFECYCLE_PROTOCOL = 2', (self.root / 'scripts/portal-macos.py').read_text())
        self.assertEqual(self.plist.read_bytes(), plist)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)
        self.assertFalse((self.root / '.portal-upgrade.json').exists())

    def test_offline_upgrade_does_not_invent_a_running_session(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        self.plist.unlink()
        (self.root / 'scripts/portal-macos.py').unlink()
        stage, _ = self.start_upgrade('unmanaged')
        result = wait_for(lambda: read(stage / 'result.json'))
        self.assertEqual(result['state'], 'succeeded')
        self.assertIn('start Portal manually', result['message'])
        self.assertEqual(upgrade.version(self.target), '0.8.1')
        self.assertFalse(self.plist.exists())
        self.assertFalse((self.root / 'scripts/portal-macos.py').exists())
        self.assertFalse(manager.checkout_pids(self.root))

    def start_legacy(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        self.plist.unlink()
        # Empty arguments and shell metacharacters must survive without parsing
        # ps output or evaluating a reconstructed shell command.
        arguments = ['--config', 'config with spaces.toml', '--name', '中文 $(touch unexpected)', '',
                     '--connect=http://127.0.0.1:9/fixture/?token=adoption-secret']
        environment = dict(os.environ, PORTAL_ADOPTION_TEST='original = value\nwith newline')
        process = subprocess.Popen([str(self.target), *arguments], cwd=self.root, env=environment,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: manager.process_identity(process.pid))
        return process, arguments, environment

    def test_live_legacy_upgrade_restarts_with_original_settings_and_one_guardian(self):
        process, arguments, environment = self.start_legacy()
        try:
            stage, _ = self.start_upgrade('adopt-legacy')
            result = wait_for(lambda: read(stage / 'result.json'))
            self.assertEqual(result['state'], 'succeeded')
            process.wait(timeout=10)
            pids = manager.checkout_pids(self.root)
            self.assertEqual(len(pids), 1)
            self.assertNotEqual(pids[0], process.pid)
            state = manager.supervisor_state(self.root)
            self.assertEqual(state['runtime']['pid'], pids[0])
            launch = manager.launch_snapshot(manager.process_identity(pids[0]))
            self.assertEqual(launch['arguments'], arguments)
            self.assertEqual(launch['cwd'], str(self.root))
            self.assertEqual(launch['environment']['PORTAL_ADOPTION_TEST'], environment['PORTAL_ADOPTION_TEST'])
            self.assertEqual(upgrade.version(self.target), '0.8.1')
            self.assertFalse(self.plist.exists())
            self.assertFalse((self.root / 'start.sh').exists())
            self.assertFalse((self.root / 'unexpected').exists())
            for path in [stage / 'request.json', stage / 'accepted.json', stage / 'worker.log',
                         self.root / '.portal-supervisor.json', self.root / 'portal-supervisor.log']:
                self.assertNotIn(b'adoption-secret', path.read_bytes())
            owner_argv = subprocess.check_output(['/bin/ps', '-p', str(state['owner']['pid']), '-o', 'command='])
            self.assertNotIn(b'adoption-secret', owner_argv)
        finally:
            manager.stop_supervisor(self.root)
            manager.stop_checkout(self.root)
            process.wait(timeout=10)

    def test_live_legacy_failed_candidate_rolls_back_and_restarts_automatically(self):
        process, _, _ = self.start_legacy()
        previous = self.target.read_bytes()
        try:
            stage, _ = self.start_upgrade('adopt-rollback', broken=True)
            result = wait_for(lambda: read(stage / 'result.json'), timeout=60)
            self.assertEqual(result['state'], 'rolled_back')
            self.assertFalse(result['restart_required'])
            self.assertEqual(self.target.read_bytes(), previous)
            self.assertEqual(len(manager.checkout_pids(self.root)), 1)
            self.assertTrue(manager.supervisor_state(self.root))
            self.assertFalse((self.root / '.portal-upgrade.json').exists())
        finally:
            manager.stop_supervisor(self.root)
            manager.stop_checkout(self.root)
            process.wait(timeout=10)

    def test_snapshot_preserves_cwd_boundaries_and_rejects_reused_identity(self):
        directory = self.root / 'cwd with spaces 中文\nand newline'
        directory.mkdir()
        arguments = ['argument with spaces', '', 'quotes " and \' and $()']
        with subprocess.Popen([str(self.target), *arguments], cwd=directory,
                              env=dict(os.environ, PORTAL_ADOPTION_TEST='kept')) as process:
            identity = wait_for(lambda: manager.process_identity(process.pid))
            try:
                snapshot = manager.launch_snapshot(identity)
                self.assertEqual(snapshot['arguments'], arguments)
                self.assertEqual(snapshot['cwd'], str(directory))
                self.assertEqual(snapshot['environment']['PORTAL_ADOPTION_TEST'], 'kept')
                with self.assertRaisesRegex(RuntimeError, 'exited'):
                    manager.launch_snapshot(dict(identity, started='different process'))
            finally:
                process.terminate()

    def test_legacy_start_script_is_preserved_and_restarts_without_supervisor(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        self.plist.unlink()
        import shlex
        start_script = self.root / 'start.sh'
        start_script.write_text('#!/bin/sh\nexec ' + shlex.quote(str(self.target)) + '\n')
        original = start_script.read_bytes()
        stage, label = self.start_upgrade('legacy-start')
        result = wait_for(lambda: read(stage / 'result.json'))
        self.assertEqual(result['state'], 'succeeded')
        # Wait until the worker process has exited, then prove the restored
        # legacy runtime survives independently of that completed launchd job.
        import re
        wait_for(lambda: not re.search(r'^\s*pid = ', manager.launchctl('print', self.domain + '/' + label).stdout, re.MULTILINE))
        time.sleep(.5)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)
        self.assertEqual(upgrade.version(self.target), '0.8.1')
        self.assertEqual(start_script.read_bytes(), original)
        self.assertFalse(self.plist.exists())

    def test_worker_waits_for_external_installer_to_exit(self):
        with subprocess.Popen(['/bin/sleep', '30']) as parent:
            previous = self.target.read_bytes()
            pids = manager.checkout_pids(self.root)
            stage, _ = self.start_upgrade('installer', parent=parent)
            wait_for(lambda: (stage / 'accepted.json').exists())
            time.sleep(1.5)
            self.assertEqual(self.target.read_bytes(), previous)
            self.assertEqual(manager.checkout_pids(self.root), pids)
            parent.terminate()
            parent.wait(timeout=5)
            wait_for(lambda: read(stage / 'result.json'))
            self.assertEqual(read(stage / 'result.json')['state'], 'succeeded')

    def test_upgrade_waits_for_supervisor_startup_lock(self):
        import fcntl
        # Portal has published readiness, but its guardian has not observed it
        # yet. This shared lock must delay the upgrade instead of rejecting it.
        with open(self.root / '.portal-upgrade.lock', 'a+b') as startup:
            fcntl.flock(startup, fcntl.LOCK_SH)
            stage, _ = self.start_upgrade('startup-window')
            time.sleep(.5)
            self.assertFalse((stage / 'error.json').exists())
            self.assertFalse((stage / 'accepted.json').exists())
        result = wait_for(lambda: read(stage / 'result.json'))
        self.assertEqual(result['state'], 'succeeded')

    def test_ad_hoc_candidate_rejection_does_not_stop_or_rewrite_portal(self):
        previous = self.target.read_bytes()
        pids = manager.checkout_pids(self.root)
        stage, _ = self.start_upgrade('reject', reject=True)
        wait_for(lambda: read(stage / 'error.json'))
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertEqual(manager.checkout_pids(self.root), pids)
        self.assertFalse((self.root / '.portal-upgrade.json').exists())

    def test_success_and_concurrent_maintenance_exclusion(self):
        previous = self.target.read_bytes()
        config = (self.root / 'portal.toml').read_bytes()
        plist = self.plist.read_bytes()
        stage, _ = self.start_upgrade('success')
        wait_for(lambda: (stage / 'accepted.json').exists())
        with self.assertRaisesRegex(RuntimeError, 'in progress'):
            with manager.maintenance_lock(self.root):
                pass
        duplicate, _ = self.start_upgrade('duplicate')
        wait_for(lambda: read(duplicate / 'error.json'))
        wait_for(lambda: read(stage / 'result.json'))
        self.assertEqual(read(stage / 'result.json')['state'], 'succeeded')
        self.assertNotEqual(self.target.read_bytes(), previous)
        self.assertEqual(self.plist.read_bytes(), plist)
        self.assertEqual((self.root / 'portal.toml').read_bytes(), config)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        self.assertEqual((stage / 'previous').read_bytes(), previous)

    def test_broken_candidate_rolls_back(self):
        previous = self.target.read_bytes()
        stage, _ = self.start_upgrade('broken', broken=True)
        wait_for(lambda: read(stage / 'result.json'), timeout=60)
        self.assertEqual(read(stage / 'result.json')['state'], 'rolled_back')
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)

    def test_killed_worker_is_restarted_and_rolls_back(self):
        previous = self.target.read_bytes()
        stage, label = self.start_upgrade('killed', broken=True)
        wait_for(lambda: read(self.root / '.portal-upgrade-status.json').get('state') == 'verifying')
        output = manager.launchctl('print', self.domain + '/' + label).stdout
        import re
        pid = int(re.search(r'^\s*pid = (\d+)', output, re.MULTILINE)[1])
        os.kill(pid, signal.SIGKILL)
        wait_for(lambda: read(stage / 'result.json'), timeout=35)
        self.assertEqual(read(stage / 'result.json')['state'], 'rolled_back')
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)

    def test_manual_recovery_after_original_supervisor_has_gone(self):
        manager.launchctl('bootout', self.service)
        manager.stop_checkout(self.root)
        self.plist.unlink()
        stage = self.root / '.portal-upgrades' / 'lost-session'
        stage.mkdir(parents=True)
        previous = self.target.read_bytes()
        (stage / 'previous').write_bytes(previous)
        (stage / 'previous').chmod(0o755)
        self.build(self.target, '0.8.1')
        upgrade.write_json(stage / 'request.json', {'root': str(self.root), 'version': '0.8.1'})
        upgrade.write_json(self.root / '.portal-upgrade.json', {'stage': str(stage), 'restart_mode': 'supervisor'})
        upgrade.run(stage)
        result = read(stage / 'result.json')
        self.assertEqual(result['state'], 'rolled_back')
        self.assertIn('manually', result['message'])
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertFalse(manager.checkout_pids(self.root))
        self.assertFalse((self.root / '.portal-upgrade.json').exists())

    def test_restored_bytes_commit_even_when_previous_runtime_cannot_restart(self):
        stage = self.root / '.portal-upgrades' / 'old-startup-failure'
        stage.mkdir(parents=True)
        previous = self.target.read_bytes()
        shutil.copy2(self.target, stage / 'previous')
        upgrade.write_json(stage / 'request.json', {'root': str(self.root), 'version': '0.8.1'})
        upgrade.write_json(self.root / '.portal-upgrade.json', {'stage': str(stage), 'restart_mode': 'launchagent'})
        with patch.object(upgrade, 'restart', side_effect=RuntimeError('previous runtime cannot start')):
            upgrade.run(stage)
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertEqual(read(stage / 'result.json')['state'], 'rolled_back')
        self.assertTrue(read(stage / 'result.json')['restart_required'])
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        self.assertFalse(manager.checkout_pids(self.root))


@unittest.skipUnless(sys.platform == 'darwin' and os.environ.get('MACOS_TEST_SIGN_IDENTITY'),
                     'optional local Developer ID signature test')
class DeveloperIDTests(unittest.TestCase):
    def test_real_signed_updates_and_incompatible_identifiers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            old, new, wrong = (root / name for name in ('old', 'new', 'wrong'))
            for index, binary in enumerate((old, new, wrong)):
                source = root / 'fixture.c'
                source.write_text(f'int main(void) {{ return {index}; }}')
                subprocess.run(['/usr/bin/clang', str(source), '-o', str(binary)], check=True, capture_output=True)
                subprocess.run(['/usr/bin/codesign', '--force', '--sign', os.environ['MACOS_TEST_SIGN_IDENTITY'],
                                '--identifier', 'com.aspect.heart-portal' if binary != wrong else 'com.aspect.wrong',
                                '--timestamp=none', '--options', 'runtime', str(binary)],
                               check=True, capture_output=True, timeout=30)
            # Real signatures and compatibility pass without notarization.
            self.assertTrue(upgrade.verify_signatures(old, new))
            legacy = root / 'legacy'
            subprocess.run(['/usr/bin/clang', str(root / 'fixture.c'), '-o', str(legacy)], check=True, capture_output=True)
            subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', str(legacy)], check=True, capture_output=True)
            self.assertFalse(upgrade.verify_signatures(legacy, new))
            self.assertFalse(upgrade.verify_signatures(wrong, new))
            with self.assertRaises(RuntimeError):
                upgrade.verify_signatures(legacy, wrong)
            with self.assertRaises(RuntimeError):
                upgrade.verify_signatures(old, wrong)
            with open(new, 'r+b') as binary:
                binary.seek(4096)
                binary.write(b'tampered')
            with self.assertRaises(RuntimeError):
                self.assertTrue(upgrade.verify_signatures(old, new))
            with self.assertRaises(RuntimeError):
                upgrade.verify_signatures(legacy, new)



if __name__ == '__main__':
    unittest.main(verbosity=2)
