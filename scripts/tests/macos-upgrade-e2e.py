#!/usr/bin/env python3
"""Exercise the original raw-binary CLI, existing LaunchAgent manager and public upgrade."""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('worker', REPO / 'scripts/portal-macos-upgrade.py')
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)
manager = worker.manager


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def wait_for(fn, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(.1)
    raise RuntimeError('Timed out waiting for local Portal lifecycle; inspect test-root logs.')


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


class Relay:
    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen()
        self.listener.settimeout(90)
        self.port = self.listener.getsockname()[1]
        self.client = None
        self.stream = None
        self.request = 0

    def disconnect(self):
        if self.stream:
            self.stream.close()
        if self.client:
            self.client.close()
        self.stream = self.client = None

    def connect(self):
        self.disconnect()
        self.client, _ = self.listener.accept()
        self.client.settimeout(360)
        self.stream = self.client.makefile('rwb', buffering=0)
        self.stream.readline()
        headers = {}
        while True:
            line = self.stream.readline().decode().strip()
            if not line:
                break
            key, value = line.split(':', 1)
            headers[key.lower()] = value.strip()
        digest = hashlib.sha1((headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()
        self.stream.write(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + base64.b64encode(digest) + b'\r\n\r\n')
        hello = self.receive()
        self.send({'ok': True, 'relay_keepalive': 'text-v1'})
        return hello

    def exact(self, count):
        data = b''
        while len(data) < count:
            chunk = self.stream.read(count - len(data))
            if not chunk:
                raise RuntimeError('Portal disconnected before its command response.')
            data += chunk
        return data

    def receive(self):
        while True:
            header = self.exact(2)
            length = header[1] & 127
            if length == 126:
                length = struct.unpack('!H', self.exact(2))[0]
            elif length == 127:
                length = struct.unpack('!Q', self.exact(8))[0]
            mask = self.exact(4) if header[1] & 128 else b''
            payload = self.exact(length)
            if mask:
                payload = bytes(value ^ mask[i % 4] for i, value in enumerate(payload))
            if header[0] & 15 == 8:
                raise RuntimeError('Portal closed WebSocket before responding.')
            if header[0] & 15 != 1:
                continue
            value = json.loads(payload)
            if value.get('type') == 'keepalive':
                self.send({'type': 'keepalive_ack'})
                continue
            return value

    def send(self, value):
        payload = json.dumps(value, separators=(',', ':')).encode()
        header = bytes([0x81, len(payload)]) if len(payload) < 126 else b'\x81\x7e' + struct.pack('!H', len(payload))
        self.stream.write(header + payload)

    def tool(self, name, arguments=None):
        self.request += 1
        self.send({'jsonrpc': '2.0', 'id': self.request, 'method': 'tools/call', 'params': {'name': name, 'arguments': arguments or {}}})
        response = self.receive()
        require(response.get('id') == self.request and 'error' not in response, f'MCP call failed: {response}')
        return response['result']

    def close(self):
        self.disconnect()
        self.listener.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True, help='The unchanged standalone release artifact.')
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--root', type=Path, default=Path.home() / '.heart-portal-user-e2e')
    parser.add_argument('--require-notarization', action='store_true')
    parser.add_argument('--lifecycle', choices=['launchagent', 'inherited', 'legacy', 'manual'], default='launchagent')
    interruption = parser.add_mutually_exclusive_group()
    interruption.add_argument('--interrupt-worker', action='store_true', help='Kill the worker after replacement and verify recovery by the existing supervisor.')
    interruption.add_argument('--interrupt-session', action='store_true', help='Kill the worker and session guardian after replacement, then recover through normal CLI startup.')
    parser.add_argument('--keep', action='store_true')
    parser.add_argument('--require-permissions', nargs='+', choices=['screen_recording', 'accessibility', 'input_monitoring'])
    args = parser.parse_args()
    require(sys.platform == 'darwin', 'macOS only')
    root = args.root.expanduser().resolve()
    original, candidate = args.binary.resolve(strict=True), args.candidate.resolve(strict=True)
    require(not root.exists() or (root / '.portal-e2e-owned').is_file(), 'Refusing to modify a directory not owned by this E2E test.')
    root.mkdir(parents=True, exist_ok=True)
    (root / '.portal-e2e-owned').touch()
    label = manager.label_for(root)
    service = f'gui/{os.getuid()}/{label}'
    plist = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
    target = root / 'heart-portal'
    report = {'mode': 'standalone-release-upgrade', 'lifecycle': args.lifecycle, 'root': str(root), 'checks': {},
              'tcc_retention': 'not_verified', 'result': 'failed',
              'notarization': 'required' if args.require_notarization else 'not_checked'}
    relay = Relay()
    direct = None
    try:
        require(worker.verify_signatures(original, candidate), 'Signed test versions must have compatible identities.')
        require(worker.version_tuple(worker.version(candidate)) > worker.version_tuple(worker.version(original)), 'Candidate must be newer.')
        if args.require_notarization:
            for binary in (original, candidate):
                worker.checked('/usr/bin/codesign', '--verify', '--strict', '--check-notarization', '-R', '=notarized', str(binary))
        subprocess.run(['/bin/sh', '-n', str(REPO / 'scripts/portal-launchagent.sh')], check=True)
        report['checks']['real_developer_id_and_mutual_requirements'] = True
        manager.stop_supervisor(root)
        worker.stop(root, service)
        target.unlink(missing_ok=True)
        shutil.copy2(original, target)
        (root / '.portal-launch-nonce').write_text('e2e-' + str(time.time_ns()))
        (root / '.portal-ready.json').unlink(missing_ok=True)
        config = root / 'portal.toml'
        config.write_text('workspace = "./workspace"\nbind = "127.0.0.1:0"\nkits_enabled = false\n')
        link = f'http://127.0.0.1:{relay.port}/e2e/?token=local-test-token'
        # Exactly the existing customer command. No installer or test-specific
        # executable logic; connection/config are explicitly supplied.
        with open(root / 'direct.log', 'wb') as log:
            direct = subprocess.Popen([str(target), '--config', str(config), '--connect', link, '--name', 'signed-e2e'],
                                      cwd=root, stdout=log, stderr=log)
        hello = relay.connect()
        direct_status = relay.tool('portal_permissions')['status']
        require(direct_status['pid'] == direct.pid, 'Original foreground CLI did not run the actual binary.')
        require(direct_status['version'] == worker.version(original), 'Foreground CLI version mismatch.')
        report['checks']['original_config_connect_name_cli'] = True
        if args.lifecycle == 'launchagent':
            direct.terminate()
            direct.wait(timeout=20)
            direct = None
            # The same existing optional management command, not an installer.
            env = dict(os.environ, PORTAL_CONNECT_LINK=link)
            command = [sys.executable, str(REPO / 'scripts/portal-macos.py'), 'install', '--root', str(root), '--name', 'signed-e2e']
            installed = subprocess.run(command, env=env, capture_output=True, text=True)
            require(installed.returncode == 0, 'LaunchAgent installation failed: ' + installed.stderr)
            require(relay.connect() == hello, 'Enabling supervision changed relay identity.')
            baseline = relay.tool('portal_permissions')['status']
            report['checks']['existing_launchagent_entry_preserves_signed_binary'] = True
        else:
            baseline = direct_status
            if args.lifecycle == 'legacy':
                (root / 'start.sh').write_text('#!/bin/sh\nexec ' + shlex.join([str(target), '--config', str(config), '--connect', link, '--name', 'signed-e2e']) + '\n')
        require(baseline['pid'] in manager.checkout_pids(root), 'Permissions must be measured in the actual Portal.')
        require(target.read_bytes() == original.read_bytes(), 'Startup changed signed bytes.')
        report['before'] = baseline
        if args.require_permissions:
            missing = [name for name in args.require_permissions if not baseline['permissions'][name]]
            require(not missing, f'Grant {missing} to {target}, then rerun. Use --keep to retain this test installation.')
        if args.lifecycle in ('launchagent', 'inherited'):
            os.kill(baseline['pid'], signal.SIGKILL)
            require(relay.connect() == hello, 'Crash recovery changed relay identity.')
            after_crash = relay.tool('portal_permissions')['status']
            require(after_crash['pid'] != baseline['pid'], 'No fresh process after crash.')
            relay.tool('portal_restart')
            require(relay.connect() == hello, 'Controlled restart changed relay identity.')
            before = relay.tool('portal_permissions')['status']
            require(before['pid'] != after_crash['pid'], 'No fresh process after controlled restart.')
            report['checks']['crash_and_controlled_restart'] = True
        names = ['portal.toml']
        if args.lifecycle == 'launchagent':
            names += ['.portal-name', '.portal-connection.url', '.portal-executable']
        elif args.lifecycle == 'legacy':
            names += ['start.sh']
        preserved = {name: (root / name).read_bytes() for name in names}
        previous_plist = plist.read_bytes() if plist.exists() else None
        response = relay.tool('portal_exec', {'command': f'{shlex.quote(str(target))} upgrade --file {shlex.quote(str(candidate))}', 'timeout_secs': 360})
        require('Upgrade accepted' in json.dumps(response), 'Public CLI did not return acceptance before shutdown: ' + json.dumps(response))
        if args.interrupt_worker or args.interrupt_session:
            require(args.lifecycle == 'inherited', 'Interruption tests require the inherited session supervisor.')
            replacing = wait_for(lambda: (value if (value := read_json(root / '.portal-upgrade-status.json')).get('state') == 'verifying' else None))
            owner = read_json(root / '.portal-upgrades' / replacing['transaction'] / 'accepted.json')['worker']
            require(str(manager.executable_path(owner['pid'])) == owner['executable'], 'Detached worker PID does not match.')
            guardian = manager.supervisor_state(root)
            if args.interrupt_session:
                require(guardian and manager.identity_alive(guardian['owner']), 'Session guardian is missing before interruption.')
                os.kill(guardian['owner']['pid'], signal.SIGKILL)
            os.kill(owner['pid'], signal.SIGKILL)
            if args.interrupt_session:
                # Simulate session teardown only for this test installation;
                # never log out the actual user or alter their TCC database.
                wait_for(lambda: not manager.supervisor_state(root))
                manager.stop_checkout(root)
                require((root / '.portal-upgrade.json').exists(), 'Transaction committed before interruption.')
                require(target.read_bytes() == candidate.read_bytes(), 'Interruption must occur after replacement.')
                if direct is not None:
                    direct.wait(timeout=20)
                with open(root / 'recovery-start.log', 'wb') as log:
                    direct = subprocess.Popen([str(target), '--config', str(config), '--connect', link, '--name', 'signed-e2e'],
                                              cwd=root, stdout=log, stderr=log)
            recovered = wait_for(lambda: (value if (value := read_json(root / '.portal-upgrade-status.json')).get('state') == 'rolled_back' else None))
            require(target.read_bytes() == original.read_bytes(), 'Recovery did not restore original signed bytes.')
            wait_for(lambda: manager.supervisor_state(root))
            # The killed candidate may have a stale socket queued in this
            # tiny relay's listen backlog. Drain it before accepting the
            # restored runtime, just as a real relay drops dead sessions.
            for attempt in range(5):
                try:
                    require(relay.connect() == hello, 'Recovery changed relay identity.')
                    restored = relay.tool('portal_permissions')['status']
                    require(restored['version'] == worker.version(original), 'Recovery connected to the wrong version.')
                    break
                except (ConnectionResetError, BrokenPipeError, RuntimeError) as error:
                    if attempt == 4 or (isinstance(error, RuntimeError) and 'disconnected before' not in str(error)):
                        raise
            require(restored['permissions'] == baseline['permissions'], 'Recovery changed TCC grants.')
            require(manager.checkout_pids(root) == [restored['pid']], 'Recovery left duplicate/missing runtimes.')
            require(not (root / '.portal-upgrade.json').exists(), 'Recovery left a blocking journal.')
            require(all((root / name).read_bytes() == value for name, value in preserved.items()), 'Recovery changed settings.')
            require(worker.verify_signatures(original, target), 'Recovery changed the original signing identity.')
            if args.interrupt_session:
                require(restored['pid'] == direct.pid, 'Normal startup did not exec the restored binary in place.')
                require(manager.supervisor_state(root)['owner'] != guardian['owner'], 'Session guardian was not replaced.')
            else:
                require(manager.supervisor_state(root)['owner'] == guardian['owner'], 'Original supervisor was lost during recovery.')
            report['checks']['session_teardown_recovers_on_normal_start' if args.interrupt_session
                             else 'detached_crash_restores_through_original_supervisor'] = True
            report['after'] = restored
            report['tcc_retention'] = {'verified_permissions': [n for n, granted in baseline['permissions'].items() if granted],
                                     'unverified_permissions': [n for n, granted in baseline['permissions'].items() if not granted]}
            report['result'] = 'passed'
            return
        if args.lifecycle != 'manual':
            require(relay.connect() == hello, 'Upgrade changed relay identity.')
        status = wait_for(lambda: (value if (value := read_json(root / '.portal-upgrade-status.json')).get('state') in ('succeeded', 'rolled_back', 'failed') else None))
        require(status['state'] == 'succeeded', f'Upgrade failed: {status}')
        if args.lifecycle == 'manual':
            require(target.read_bytes() == candidate.read_bytes(), 'Manual update changed signed bytes.')
            require(not manager.checkout_pids(root), 'Manual update unexpectedly started a runtime.')
            require(not plist.exists(), 'Manual update installed a supervisor.')
            require(all((root / name).read_bytes() == value for name, value in preserved.items()), 'Manual update changed config.')
            report['upgrade'] = 'passed'
            report['checks']['unmanaged_upgrade_preserves_manual_start'] = True
            report['result'] = 'passed'
            return
        after = relay.tool('portal_permissions')['status']
        require(after['version'] == worker.version(candidate), 'New Portal version mismatch.')
        require(target.read_bytes() == candidate.read_bytes(), 'Installed signed bytes differ from candidate.')
        require(manager.checkout_pids(root) == [after['pid']], 'Duplicate/missing runtime after upgrade.')
        require(all((root / name).read_bytes() == value for name, value in preserved.items()), 'Upgrade changed settings.')
        require((plist.read_bytes() if plist.exists() else None) == previous_plist, 'Upgrade changed the existing LaunchAgent definition.')
        subprocess.run([str(target), 'upgrade', '--status'], check=True, capture_output=True)
        require(worker.verify_signatures(original, target), 'Installed signing identity changed.')
        report['after'] = after
        report['upgrade'] = 'passed'
        granted = [name for name, allowed in baseline['permissions'].items() if allowed]
        require(all(after['permissions'][name] for name in granted), 'An existing TCC permission was lost after upgrade.')
        report['tcc_retention'] = {'verified_permissions': granted, 'unverified_permissions': [n for n in baseline['permissions'] if n not in granted]}
        report['checks']['public_cli_upgrade_preserves_path_settings_signature_and_lifecycle'] = True
        report['result'] = 'passed'
    finally:
        if direct is not None and direct.poll() is None:
            direct.terminate()
            direct.wait(timeout=20)
        relay.close()
        if not args.keep:
            manager.stop_supervisor(root)
            worker.stop(root, service)
            plist.unlink(missing_ok=True)
        (root / 'verification-report.json').write_text(json.dumps(report, indent=2))
        print(json.dumps(report, indent=2))
        if args.keep:
            print(f'Test service retained at {target}.')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'E2E failed: {error}', file=sys.stderr)
        sys.exit(1)
