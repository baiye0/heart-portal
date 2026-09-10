"""Embedded normal-start recovery bridge; no new job or persisted credentials."""
import json
import os
from pathlib import Path
import runpy
import sys


def main():
    root, stage, target = map(Path, sys.argv[1:4])
    arguments = sys.argv[4:]
    worker = stage / 'portal-macos-upgrade.py'
    # Use the transaction's saved recovery implementation, including journals
    # created by older releases. Never treat the user's CLI flags as worker flags.
    sys.argv = [str(worker)]
    saved = runpy.run_path(str(worker))
    if saved['main']() != 0:
        raise RuntimeError(f'Recovery did not finish; see {stage / "worker.log"} and retry normal startup.')
    result = json.loads((stage / 'result.json').read_text())
    if result['state'] not in ('rolled_back', 'succeeded'):
        raise RuntimeError(f'Recovery ended with {result["state"]}; see {stage / "worker.log"}.')
    manager = saved['manager']
    with manager.maintenance_lock(root):
        if (root / '.portal-upgrade.json').exists():
            raise RuntimeError('Another upgrade needs recovery; retry normal startup.')
        service = f'gui/{os.getuid()}/' + manager.label_for(root)
        if (manager.supervisor_state(root) or manager.checkout_pids(root)
                or manager.launchctl('print', service, check=False).returncode == 0):
            print('Upgrade recovered; the existing lifecycle owns Portal startup.', file=sys.stderr)
            return
        print('Upgrade recovered; continuing the original Portal command.', file=sys.stderr, flush=True)
        # Python's lock fd is non-inheritable. Exec releases it and reloads the
        # restored executable, retaining this PID, cwd, environment and TCC origin.
        os.execv(str(target), [str(target), *arguments])


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Upgrade recovery: {error}', file=sys.stderr)
        sys.exit(1)
