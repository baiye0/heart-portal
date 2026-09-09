#!/usr/bin/env python3
"""Recoverable macOS worker: separate job when supervised, same TCC origin otherwise."""
import importlib.util
import json
import hashlib
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('manager', Path(__file__).with_name('portal-macos.py'))
manager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manager)

# Keep candidate trust aligned with release.yml. Old identity differences are
# reported, not upgrade blockers: released 0.8.0 re-signs itself ad-hoc on startup.
RELEASE_REQUIREMENT = ('anchor apple generic and identifier "com.aspect.heart-portal" '
                       'and certificate leaf[subject.OU] = "7N8XHQWCNN" '
                       'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists')


def write_json(path, value):
    manager.private_write(path, json.dumps(value).encode())


def checked(*command):
    result = subprocess.run(command, capture_output=True, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError(f'{Path(command[0]).name} verification failed: {result.stderr.strip()}')
    return result.stdout + result.stderr


def requirement(binary):
    text = checked('/usr/bin/codesign', '-d', '-r-', str(binary))
    match = re.search(r'^designated => (.+)$', text, re.MULTILINE)
    if not match:
        raise RuntimeError('Missing stable designated requirement; ad-hoc/unsigned upgrades cannot preserve TCC permissions.')
    return match[1]


def verify_signatures(target, candidate):
    checked('/usr/bin/codesign', '--verify', '--strict', '-R', '=' + RELEASE_REQUIREMENT, str(candidate))
    try:
        checked('/usr/bin/codesign', '--verify', '--strict', '-R', '=' + RELEASE_REQUIREMENT, str(target))
        old, new = requirement(target), requirement(candidate)
        checked('/usr/bin/codesign', '--verify', '--strict', '-R', '=' + old, str(candidate))
        checked('/usr/bin/codesign', '--verify', '--strict', '-R', '=' + new, str(target))
    except RuntimeError:
        return False
    return True


def version(binary):
    text = checked(str(binary), '--version').strip()
    if not re.fullmatch(r'heart-portal \d+\.\d+\.\d+', text):
        raise RuntimeError('Candidate is not a versioned Heart Portal release.')
    return text.split()[1]


def version_tuple(value):
    return tuple(map(int, value.split('.')))


def wait_ready(root, expected, nonce=None, timeout=35):
    deadline = time.monotonic() + timeout
    stable_pid, stable_since = None, None
    while time.monotonic() < deadline:
        pids = manager.checkout_pids(root)
        valid = len(pids) == 1
        if nonce and valid:
            try:
                ready = json.loads((root / '.portal-ready.json').read_text())
                valid = (ready.get('pid') == pids[0] and ready.get('nonce') == nonce
                         and ready.get('version') == expected)
            except (OSError, ValueError):
                valid = False
        if valid:
            if stable_pid != pids[0]:
                stable_pid, stable_since = pids[0], time.monotonic()
            if time.monotonic() - stable_since >= 2:
                return
        else:
            stable_pid, stable_since = None, None
        time.sleep(0.1)
    raise RuntimeError('Portal failed local startup verification; see portal-runtime.err.log.')


def stop(root, service):
    if manager.launchctl('print', service, check=False).returncode == 0:
        manager.launchctl('bootout', service)
    manager.stop_checkout(root)


def restart(root, domain, plist, mode, expected=None, nonce=None):
    if mode == 'launchagent':
        manager.launchctl('bootstrap', domain, str(plist))
        wait_ready(root, expected, nonce)
    elif mode == 'supervisor':
        state = manager.supervisor_state(root)
        if not state:
            raise RuntimeError('The original supervisor exited; restart from the original Terminal/app to preserve TCC attribution.')
        journal = json.loads((root / '.portal-upgrade.json').read_text())
        write_json(root / '.portal-supervisor-restart.json', {
            'transaction': Path(journal['stage']).name, 'id': uuid.uuid4().hex, 'owner': state['token']})
        wait_ready(root, expected, nonce)
    elif mode == 'start_script':
        # Preserve the pre-existing installer's start.sh entry and settings.
        # The independent worker abandons child groups when it exits so this
        # legacy runtime survives without installing a new Portal supervisor.
        with open(root / 'portal-upgrade-restart.log', 'ab') as log:
            subprocess.Popen(['/bin/sh', str(root / 'start.sh')], cwd=root,
                             stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                             env=dict(os.environ, HEART_PORTAL_UPGRADE_START=manager.saved(root, '.portal-launch-nonce')),
                             start_new_session=True)
        wait_ready(root, None)
    elif mode != 'manual':
        raise RuntimeError('Unknown restart mode in upgrade journal.')


def restore(root, target, stage, service, domain, plist, mode):
    stop(root, service)
    # Copy to a fresh inode before atomic rename; never overwrite mapped code.
    temporary = target.with_name('.heart-portal.restore')
    shutil.copy2(stage / 'previous', temporary)
    with open(temporary, 'rb') as backup:
        os.fsync(backup.fileno())
    os.replace(temporary, target)
    if mode == 'supervisor' and not manager.supervisor_state(root):
        # Restoring the signed bytes succeeded. A vanished permission owner must
        # not leave a recoverable installation stuck behind its journal forever.
        return False
    if mode == 'start_script':
        manager.private_write(root / '.portal-launch-nonce', stage.name.encode())
    try:
        restart(root, domain, plist, mode)  # Older binaries may not publish readiness.
    except Exception as error:
        print(f'Previous binary restored, but restart failed: {error}', file=sys.stderr, flush=True)
        if mode == 'supervisor':
            manager.stop_supervisor(root)
        stop(root, service)
        return False
    return mode != 'manual'


def run(stage):
    request = json.loads((stage / 'request.json').read_text())
    root = Path(request['root']).resolve(strict=True)
    target = manager.binary_path(root)
    if request.get('target') and Path(request['target']).resolve() != target.resolve():
        raise RuntimeError('Upgrade must be invoked from this installation\'s actual executable.')
    label = manager.label_for(root)
    domain = f'gui/{os.getuid()}'
    service = f'{domain}/{label}'
    plist = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
    journal = root / '.portal-upgrade.json'
    result_file = stage / 'result.json'
    if result_file.exists():
        return
    # A just-restarted Portal can answer tools before its supervisor observes
    # readiness and releases the shared startup lock. Wait for that short window;
    # an existing upgrade journal still rejects overlapping transactions at once.
    with manager.maintenance_lock(root, timeout=3):
        def status(state, message, **extra):
            write_json(root / '.portal-upgrade-status.json', {'state': state, 'message': message,
                       'version': request.get('version'), 'transaction': stage.name,
                       'signature_identity_preserved': request.get('signature_identity_preserved'),
                       'permission_notice': ('Signing identity changed; macOS may require one-time authorization.'
                                             if request.get('signature_identity_preserved') is False else None), **extra})

        def finish(state, message, **extra):
            status(state, message, **extra)
            # Durable outcome before removing the journal: recovery can repeat
            # rollback after a crash, but must never replay a committed upgrade.
            write_json(result_file, {'state': state, 'message': message, **extra})
            journal.unlink(missing_ok=True)

        manager.assert_owned(plist, root, label)
        if journal.exists():
            previous = json.loads(journal.read_text())
            if previous.get('stage') != str(stage):
                raise RuntimeError('Another interrupted upgrade needs recovery first.')
            mode = previous.get('restart_mode', 'launchagent')
            if mode == 'supervisor' and not manager.supervisor_state(root):
                # A manual recovery must not invent a different permission owner.
                mode = 'manual'
            status('rolling_back', 'Recovering interrupted upgrade.')
            restarted = restore(root, target, stage, service, domain, plist, mode)
            finish('rolled_back', 'Interrupted upgrade recovered; previous binary restored.' +
                   (' Previous Portal is running.' if restarted else ' Start Portal manually with its original command.'),
                   restart_required=not restarted)
            return
        try:
            candidate = stage / 'candidate'
            # All rejection checks precede stopping the service or mutating its binary.
            request['signature_identity_preserved'] = verify_signatures(target, candidate)
            verified_hash = hashlib.sha256(candidate.read_bytes()).digest()
            old_version, new_version = version(target), version(candidate)
            if version_tuple(new_version) <= version_tuple(old_version):
                raise RuntimeError('Candidate must be newer than the installed Portal.')
            if request.get('version') and new_version != request['version']:
                raise RuntimeError('Candidate version differs from release metadata.')
            supervised = manager.launchctl('print', service, check=False).returncode == 0
            if 'was_supervised' in request and request['was_supervised'] != supervised:
                raise RuntimeError('Supervision changed while submitting the upgrade; runtime was left unchanged. Retry.')
            if supervised and not plist.exists():
                raise RuntimeError('Loaded Portal LaunchAgent has no owned plist; refusing to replace it.')
            mode = ('launchagent' if supervised else 'supervisor' if manager.supervisor_state(root)
                    else 'start_script' if (root / 'start.sh').is_file() else 'manual')
            helper = root / 'scripts/portal-macos.py'
            if supervised and (not helper.is_file() or 'LIFECYCLE_PROTOCOL = 2' not in helper.read_text()):
                # Existing installations gain the shared lock in place; users
                # do not need a new installer or reinstall before upgrading.
                manager.private_write(helper, Path(__file__).with_name('portal-macos.py').read_bytes())
            request['version'] = new_version
            write_json(stage / 'request.json', request)
            shutil.copy2(target, stage / 'previous')
            with open(stage / 'previous', 'rb') as backup:
                os.fsync(backup.fileno())
        except Exception as error:
            status('failed', str(error))
            raise
        write_json(journal, {'stage': str(stage), 'restart_mode': mode})
        status('accepted', 'Independent upgrade worker accepted the upgrade.')
        write_json(stage / 'accepted.json', {'version': new_version, 'worker': manager.process_identity(os.getpid()),
                   'signature_identity_preserved': request['signature_identity_preserved']})
        # Allow the calling CLI to return through Portal before bootout kills
        # Portal's entire process group (including any calling kit/exec child).
        deadline = time.monotonic() + 20
        parent_executable = Path(request.get('parent_executable', str(target)))
        while manager.executable_path(request['parent_pid']) == parent_executable:
            if time.monotonic() >= deadline:
                finish('failed', 'Calling upgrade command did not exit; runtime was left unchanged.')
                return
            time.sleep(0.1)
        time.sleep(1)
        try:
            status('replacing', 'Stopping this installation and replacing its executable.')
            stop(root, service)
            # Recheck immediately before replacement; never re-sign or strip xattrs.
            if hashlib.sha256(candidate.read_bytes()).digest() != verified_hash:
                raise RuntimeError('Candidate changed after signature verification.')
            manager.private_write(root / '.portal-launch-nonce', stage.name.encode())
            (root / '.portal-ready.json').unlink(missing_ok=True)
            os.replace(candidate, target)
            if mode != 'manual':
                status('verifying', 'Waiting for local startup readiness.')
            restart(root, domain, plist, mode, new_version, stage.name)
            message = ('Binary updated; no start.sh or active LaunchAgent, so start Portal manually.'
                       if mode == 'manual' else 'New Portal is locally ready; original launch settings and release signature preserved.')
            finish('succeeded', message)
        except Exception:
            status('rolling_back', 'New Portal failed; restoring previous executable.')
            restarted = restore(root, target, stage, service, domain, plist, mode)
            finish('rolled_back', 'Upgrade failed; previous binary restored.' +
                   (' Previous Portal is running.' if restarted else ' Start Portal manually with its original command.') + ' See worker.log for details.',
                   restart_required=not restarted)
            raise


def dispatch(stage):
    request = json.loads((stage / 'request.json').read_text())
    root = Path(request['root'])
    domain = f'gui/{os.getuid()}'
    supervised = manager.launchctl('print', domain + '/' + manager.label_for(root), check=False).returncode == 0
    request['was_supervised'] = supervised
    write_json(stage / 'request.json', request)
    arguments = [sys.executable, str(stage / 'portal-macos-upgrade.py')]
    if not supervised:
        # Keep the original Terminal/app TCC responsibility. The existing session
        # supervisor recovers interrupted work; a second recovery watcher is unnecessary.
        with open(stage / 'worker.log', 'ab') as log:
            subprocess.Popen(arguments, cwd=stage, stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=True)
        return
    # A sibling launchd job survives bootout of Portal's process group.
    plist = Path(request['worker_plist'])
    manager.private_write(plist, plistlib.dumps({
        'Label': plist.stem, 'ProgramArguments': arguments, 'RunAtLoad': True,
        'KeepAlive': {'SuccessfulExit': False}, 'ThrottleInterval': 10,
        'AbandonProcessGroup': True, 'WorkingDirectory': str(stage), 'Umask': 63,
        'StandardOutPath': str(stage / 'worker.log'), 'StandardErrorPath': str(stage / 'worker.log')}))
    try:
        manager.launchctl('bootstrap', domain, str(plist))
    except Exception:
        plist.unlink(missing_ok=True)
        raise


def main():
    stage = Path(__file__).resolve().parent
    if '--dispatch' in sys.argv:
        dispatch(stage)
        return 0
    request = json.loads((stage / 'request.json').read_text())
    try:
        run(stage)
    except Exception as error:
        print(f'Upgrade: {error}', file=sys.stderr, flush=True)
        journal = Path(request['root']) / '.portal-upgrade.json'
        owns_journal = journal.exists() and json.loads(journal.read_text()).get('stage') == str(stage)
        if not (stage / 'accepted.json').exists() and not owns_journal:
            write_json(stage / 'error.json', {'message': str(error)})
            # Rejected requests should not be retried automatically.
            write_json(stage / 'result.json', {'state': 'failed', 'message': str(error)})
        elif not (stage / 'result.json').exists():
            # The existing supervisor (or launchd KeepAlive) retries this worker;
            # the journal forces rollback before any further candidate execution.
            return 1
    if (stage / 'result.json').exists():
        journal = Path(request['root']) / '.portal-upgrade.json'
        if journal.exists() and json.loads(journal.read_text()).get('stage') == str(stage):
            journal.unlink()
        # Remove login registration only after a durable result. The completed
        # job stays loaded but dormant (SuccessfulExit=false); it owns no process.
        Path(request['worker_plist']).unlink(missing_ok=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
