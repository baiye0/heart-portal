#!/usr/bin/env python3
"""Config CLI regression in an isolated home; never starts Portal or contacts a relay.

PORTAL_TEST_BINARY=/absolute/path/to/heart-portal python scripts/tests/config-layout.tests.py
"""
import json
import importlib.util
from contextlib import ExitStack
import sys
from types import SimpleNamespace
from unittest.mock import patch
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', str(
    REPO / 'target/debug' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
))).resolve()


class ConfigCliTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="portal config test ' ")
        self.root = Path(self.temporary.name).resolve()
        assert self.root.parent == Path(tempfile.gettempdir()).resolve()
        assert self.root.name.startswith("portal config test ' ")
        self.addCleanup(self.temporary.cleanup)
        self.install = self.root / 'download'
        self.install.mkdir()
        self.binary = self.install / BINARY.name
        shutil.copy2(BINARY, self.binary)
        self.home = self.root / 'home'
        self.data = self.home / '.heart-portal'
        self.env = os.environ.copy()
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home), PORTAL_CONNECT_LINK='')

    def run_cli(self, *args, success=True):
        state = self.install / '.portal-runtime.json'
        previous_state = state.read_bytes() if state.exists() else None
        result = subprocess.run([str(self.binary), *map(str, args)], cwd=self.install,
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        self.assertNotIn('fixture-private-token', result.stdout + result.stderr)
        self.assertEqual(state.read_bytes() if state.exists() else None, previous_state)
        return json.loads(result.stdout) if success else result

    def test_inspection_and_legacy_precedence_without_bootstrap(self):
        location = self.run_cli('config', 'path')['config']
        self.assertEqual(Path(location['path']), self.data / 'portal.toml')
        self.assertFalse(self.home.exists())
        self.data.mkdir(parents=True)
        central = self.data / 'portal.toml'
        central.write_text("name='central'\n", encoding='utf-8')
        legacy = self.install / 'portal.toml'
        legacy.write_text("name='legacy'\n", encoding='utf-8')
        self.assertEqual(Path(self.run_cli('config', 'path')['config']['path']), legacy)
        self.assertEqual(Path(self.run_cli('--config', central, 'config', 'path')['config']['path']), central)
        self.run_cli('--config', self.root / 'missing.toml', 'config', 'path', success=False)

    @unittest.skipUnless(sys.platform in ('win32', 'darwin'), 'desktop installation')
    def test_user_runtime_is_atomic_and_download_stays_clean(self):
        with ThreadPoolExecutor(max_workers=3) as pool:
            installed = list(pool.map(lambda _: self.run_cli('--install-user-runtime'), range(3)))
        target = self.data / 'runtime' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
        self.assertEqual({Path(value['executable']) for value in installed}, {target})
        self.assertEqual(target.read_bytes(), self.binary.read_bytes())
        self.assertEqual(list(self.install.iterdir()), [self.binary])
        self.assertFalse((target.parent / '.portal-runtime.json').exists())
        self.assertFalse(list(target.parent.glob('.install-*.tmp')))
        if os.name != 'nt':
            self.assertTrue(os.access(target, os.X_OK))

    @unittest.skipUnless(sys.platform in ('win32', 'darwin'), 'desktop installation')
    def test_user_installation_freezes_legacy_config_and_rejects_conflicts(self):
        legacy = self.install / 'portal.toml'
        original = "name='legacy'\nworkspace='./workspace'\nkits_dir='./kits'\n"
        legacy.write_text(original, encoding='utf-8')
        (self.install / 'kits').mkdir()
        self.data.mkdir(parents=True)
        central = self.data / 'portal.toml'
        central.write_text("name='another-being'\n", encoding='utf-8')
        self.run_cli('--install-user-runtime', success=False)
        self.assertEqual(central.read_text(), "name='another-being'\n")
        self.assertFalse((self.data / 'runtime' / self.binary.name).exists())
        central.unlink()
        self.run_cli('--install-user-runtime')
        self.assertEqual(legacy.read_text(), original)
        import tomllib
        migrated = tomllib.loads(central.read_text(encoding='utf-8'))
        self.assertEqual(Path(migrated['workspace']), self.install / 'workspace')
        self.assertEqual(Path(migrated['kits_dir']), self.install / 'kits')
        self.assertEqual(migrated['name'], 'legacy')
        self.assertEqual(Path(self.run_cli('config', 'path')['config']['path']), central)

    @unittest.skipUnless(os.name == 'nt', 'Windows process identity')
    def test_live_legacy_supervisor_blocks_migration_without_changing_it(self):
        powershell = Path(os.environ['SystemRoot']) / 'System32/WindowsPowerShell/v1.0/powershell.exe'
        ticks = subprocess.check_output([str(powershell), '-NoProfile', '-NonInteractive', '-Command',
            '(Get-Process -Id $env:PORTAL_FIXTURE_PID).StartTime.ToUniversalTime().Ticks'],
            env=dict(self.env, PORTAL_FIXTURE_PID=str(os.getpid())), text=True).strip()
        state = self.install / '.portal-runtime.json'
        original = json.dumps({'supervisor_pid': os.getpid(), 'supervisor_started': int(ticks)})
        state.write_text(original)
        failed = self.run_cli('--install-user-runtime', success=False)
        self.assertIn('legacy Portal or its login supervisor is still active', failed.stderr)
        self.assertEqual(state.read_text(), original)
        self.assertFalse((self.data / 'runtime' / 'heart-portal.exe').exists())
        state.write_text(json.dumps({'supervisor_pid': 2147483647, 'supervisor_started': 0}))
        self.run_cli('--install-user-runtime')

    def test_init_creates_private_central_config_and_preserves_edits(self):
        self.env['HEART_PORTAL_EXTERNAL_TOOL'] = '1'
        self.run_cli('config', 'init', success=False)
        self.assertFalse(self.home.exists())
        self.env.pop('HEART_PORTAL_EXTERNAL_TOOL')
        location = self.run_cli('config', 'init')['config']
        central = self.data / 'portal.toml'
        self.assertEqual(Path(location['path']), central)
        self.assertFalse((self.install / 'portal.toml').exists())
        self.assertFalse((self.install / 'workspace').exists())
        self.assertIn('127.0.0.1:9100', central.read_text(encoding='utf-8'))
        if os.name != 'nt':
            self.assertEqual(central.stat().st_mode & 0o777, 0o600)
        original = b"name='user-edited'\nportal_mcp_token='fixture-private-token'\n"
        central.write_bytes(original)
        self.run_cli('config', 'init')
        self.assertEqual(central.read_bytes(), original)
        self.assertFalse((self.data / 'workspace').exists())

    def test_concurrent_initializers_publish_one_complete_config(self):
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: self.run_cli('config', 'init'), range(8)))
        self.assertTrue(all(Path(result['config']['path']) == self.data / 'portal.toml' for result in results))
        self.assertFalse(list(self.data.glob('.portal-config-*.tmp')))
        self.assertFalse((self.install / 'portal.toml').exists())

    def test_init_does_not_repair_or_replace_invalid_config(self):
        self.data.mkdir(parents=True)
        central = self.data / 'portal.toml'
        original = b"invalid fixture-private-token"
        central.write_bytes(original)
        self.run_cli('config', 'init', success=False)
        self.assertEqual(central.read_bytes(), original)

    def test_macos_installer_preserves_central_config_during_rollback(self):
        # Run the installer file/config logic on every OS; native metadata reads,
        # process ownership and launchd have separate platform-specific coverage.
        spec = importlib.util.spec_from_file_location('portal_macos_config_test', REPO / 'scripts/portal-macos.py')
        manager = importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules, {'fcntl': SimpleNamespace()}):
            spec.loader.exec_module(manager)
        central = Path(self.run_cli('config', 'init')['config']['path'])
        original = central.read_bytes()
        label = 'config-fixture'
        plist = self.root / 'launchagents' / (label + '.plist')
        args = SimpleNamespace(name='fixture', connect_link='https://relay.invalid/fixture/?token=fixture-private-token', config=central)
        real_run = subprocess.run
        with ExitStack() as stack:
            if os.name == 'nt':
                # This fixture uses ordinary files. Keep testing rollback on
                # Windows without invoking the macOS-only O_NOFOLLOW/O_NONBLOCK
                # reader; its link/size guards are tested in runtime-security.
                stack.enter_context(patch.object(manager, 'metadata_bytes', side_effect=Path.read_bytes))
            stack.enter_context(patch.object(manager, 'binary_path', return_value=self.binary))
            stack.enter_context(patch.object(manager, 'stop_supervisor'))
            stack.enter_context(patch.object(manager, 'stop_checkout'))
            stack.enter_context(patch.object(manager, 'checkout_pids', return_value=[]))
            stack.enter_context(patch.object(manager.subprocess, 'run', side_effect=lambda *a, **kw: real_run(*a, env=self.env, **kw)))
            def launchctl(*arguments, **kwargs):
                if arguments[0] == 'bootstrap':
                    # Concurrent user edits must survive installer rollback.
                    central.write_bytes(original + b'\n# user edit during installation\n')
                    raise RuntimeError('simulated bootstrap failure')
                return SimpleNamespace(returncode=0 if len(arguments) == 2 and arguments[1] == 'gui/fixture' else 1)

            stack.enter_context(patch.object(manager, 'launchctl', side_effect=launchctl))
            with self.assertRaisesRegex(RuntimeError, 'simulated bootstrap failure'):
                manager.install(args, self.install, plist, label, 'gui/fixture', 'gui/fixture/' + label)
            self.assertEqual(central.read_bytes(), original + b'\n# user edit during installation\n')
            self.assertFalse(plist.exists())
            self.assertFalse((self.install / 'portal.toml').exists())
            self.assertFalse((self.install / 'workspace').exists())
            # Both old registrations and explicit user-directory registrations
            # retain strict checkout ownership checks.
            manager.private_write(plist, manager.plistlib.dumps(manager.definition(self.install, label, central)))
            manager.assert_owned(plist, self.install, label)
            wrong = manager.definition(self.install, label, central)
            wrong['ProgramArguments'][1] += '.other'
            manager.private_write(plist, manager.plistlib.dumps(wrong))
            with self.assertRaisesRegex(RuntimeError, 'another checkout'):
                manager.assert_owned(plist, self.install, label)

    def test_preview_apply_conflict_and_idempotence(self):
        source = self.install / 'portal.toml'
        original = "name='legacy'\nworkspace='./workspace'\nkits_dir='./kits'\nportal_mcp_token='fixture-private-token'\n"
        source.write_text(original, encoding='utf-8')
        args = ('config', 'migrate', '--from', source, '--profile', 'desktop')
        preview = self.run_cli(*args)
        destination = Path(preview['migration']['destination'])
        self.assertFalse(destination.exists())
        self.assertFalse(self.home.exists())
        self.run_cli(*args, '--apply')
        self.assertTrue(destination.is_file())
        self.assertEqual(source.read_text(encoding='utf-8'), original)
        self.assertTrue(self.run_cli(*args, '--apply')['migration']['already_applied'])
        before = destination.read_bytes()
        source.write_text("name='changed'\nworkspace='./elsewhere'\n", encoding='utf-8')
        self.run_cli(*args, '--apply', success=False)
        self.assertEqual(destination.read_bytes(), before)

    def test_saved_external_config_is_retained_and_missing_saved_path_fails(self):
        external = self.root / 'external.toml'
        external.write_text("name='external'\n", encoding='utf-8')
        (self.install / '.portal-launch.json').write_text(json.dumps({
            'arguments': ['--config', str(external)],
            'environment': {'PORTAL_CONNECT_LINK': 'https://relay.invalid/fixture/?token=fixture-private-token'},
        }), encoding='utf-8')
        location = self.run_cli('config', 'path')['config']
        self.assertEqual(location['source'], 'saved-launch')
        self.assertEqual(Path(location['path']), external)
        # The config is external, but its effective credentials live beside exe.
        migration = self.run_cli('config', 'migrate', '--from', external, '--apply')
        destination = Path(migration['migration']['destination'])
        self.assertIn('fixture-private-token', destination.read_text(encoding='utf-8'))
        external.unlink()
        self.run_cli('config', 'path', success=False)

    def test_explicit_installation_preserves_external_identity_and_rejects_mismatch(self):
        old = self.root / 'old-installation'
        old.mkdir()
        source = self.root / 'external.toml'
        source.write_text("name='external'\n", encoding='utf-8')
        record = old / '.portal-launch.json'
        record.write_text(json.dumps({
            'arguments': ['--config', str(source)], 'name': 'saved-identity',
            'environment': {'PORTAL_CONNECT_LINK': 'https://relay.invalid/fixture/?token=fixture-private-token'},
        }), encoding='utf-8')
        args = ('config', 'migrate', '--from', source, '--installation', old)
        preview = self.run_cli(*args)
        self.assertFalse(self.home.exists())
        self.run_cli(*args, '--apply')
        destination = Path(preview['migration']['destination'])
        self.assertIn('saved-identity', destination.read_text(encoding='utf-8'))
        record.write_text(json.dumps({'arguments': ['--config', str(old / 'different.toml')]}), encoding='utf-8')
        self.run_cli(*args, '--profile', 'other', success=False)
        record.write_text('invalid fixture-private-token', encoding='utf-8')
        self.run_cli(*args, '--profile', 'other', success=False)


if __name__ == '__main__':
    unittest.main()
