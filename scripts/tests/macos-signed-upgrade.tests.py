#!/usr/bin/env python3
"""Validate a signed release through the real CLI and a local relay.

The candidate is an isolated source copy with only its version incremented.
It is temporary test data and must never be uploaded as a release artifact.
"""
import argparse
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--keychain', type=Path)
    parser.add_argument('--build-dir', type=Path, default=REPO / 'target/portal-macos-local-build')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    subprocess.run([sys.executable, str(REPO / 'scripts/package-portal-macos.py'),
                    '--verify-only', str(binary)], check=True)
    version = re.search(r'^version = "([^"]+)"', (REPO / 'portal/Cargo.toml').read_text(), re.MULTILINE)[1]
    major, minor, patch = map(int, version.split('.'))
    candidate_version = f'{major}.{minor}.{patch + 1}'
    with tempfile.TemporaryDirectory(prefix='portal-signed-upgrade-') as temporary:
        root = Path(temporary)
        source = root / 'source'
        source.mkdir()
        for name in ('Cargo.toml', 'Cargo.lock', 'portal.example.toml', 'LICENSE'):
            shutil.copy2(REPO / name, source)
        for name in ('portal', 'scripts'):
            shutil.copytree(REPO / name, source / name, ignore=shutil.ignore_patterns('target', '__pycache__'))
        for path, before, after in (
            (source / 'portal/Cargo.toml', f'version = "{version}"', f'version = "{candidate_version}"'),
            (source / 'Cargo.lock', f'name = "heart-portal"\nversion = "{version}"',
             f'name = "heart-portal"\nversion = "{candidate_version}"')):
            content = path.read_text()
            if content.count(before) != 1:
                raise RuntimeError('Ambiguous test version substitution.')
            path.write_text(content.replace(before, after, 1))
        build = args.build_dir.resolve()
        subprocess.run(['cargo', 'build', '--release', '--locked', '--manifest-path',
                        str(source / 'Cargo.toml'), '--target-dir', str(build)], check=True)
        signing = [sys.executable, str(source / 'scripts/package-portal-macos.py'),
                   '--binary', str(build / 'release/heart-portal'), '--output', str(root / 'signed')]
        if args.keychain:
            signing += ['--keychain', str(args.keychain)]
        subprocess.run(signing, check=True)
        slug = 'macos-arm64' if platform.machine() == 'arm64' else 'macos-x86_64'
        candidate = root / 'signed' / f'heart-portal-{slug}'
        for case, options in [('upgrade', []), ('session-recovery', ['--interrupt-session'])]:
            subprocess.run([sys.executable, str(REPO / 'scripts/tests/macos-upgrade-e2e.py'),
                            '--binary', str(binary), '--candidate', str(candidate),
                            '--lifecycle', 'inherited', '--root', str(root / case), *options], check=True)
    print('PASS: signed CLI upgrade, automatic restart and interrupted-session recovery')


if __name__ == '__main__':
    main()
