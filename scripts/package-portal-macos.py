#!/usr/bin/env python3
"""Build and Developer ID sign the original standalone macOS release binary."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys

REPO = Path(__file__).resolve().parents[1]
IDENTITY = 'Developer ID Application: D5 Inc. (7N8XHQWCNN)'
IDENTIFIER = 'com.aspect.heart-portal'
REQUIREMENT = ('=anchor apple generic and identifier "com.aspect.heart-portal" '
               'and certificate leaf[subject.OU] = "7N8XHQWCNN" '
               'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists')


def run(args, timeout=900, stdout_only=False):
    try:
        result = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f'{Path(args[0]).name} timed out after {timeout}s') from None
    if result.returncode:
        raise RuntimeError(f'{Path(args[0]).name} failed: {result.stderr.strip()}')
    return result.stdout if stdout_only else result.stdout + result.stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=['aarch64-apple-darwin', 'x86_64-apple-darwin'],
                        default='aarch64-apple-darwin' if platform.machine() == 'arm64' else 'x86_64-apple-darwin')
    parser.add_argument('--binary', type=Path, help='Sign a copy of an already built release executable.')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--keychain', type=Path)
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('Run on macOS.')
    version = re.search(r'^version = "([^"]+)"', (REPO / 'portal/Cargo.toml').read_text(), re.MULTILINE)[1]
    slug = 'macos-arm64' if args.target.startswith('aarch64') else 'macos-x86_64'
    out = (args.output or REPO / 'dist' / 'signed-unnotarized' / slug).resolve()
    out.mkdir(parents=True, exist_ok=True)
    if args.binary:
        source = args.binary.resolve(strict=True)
    else:
        build = REPO / 'target/portal-package-macos'
        print(f'Building {version} in isolated directory {build}', flush=True)
        run(['cargo', 'build', '--release', '--locked', '--manifest-path', REPO / 'Cargo.toml', '--target', args.target, '--target-dir', build])
        source = build / args.target / 'release/heart-portal'
    expected_arch = 'arm64' if args.target.startswith('aarch64') else 'x86_64'
    if run(['/usr/bin/lipo', '-archs', source]).strip().split() != [expected_arch]:
        raise RuntimeError('Binary architecture does not match the requested target.')
    if run([source, '--version']).strip() != f'heart-portal {version}':
        raise RuntimeError('Build version differs from source.')
    raw = out / f'heart-portal-{slug}'
    if source == raw:
        raise RuntimeError('Use a separate output directory; never sign a running executable in place.')
    shutil.copy2(source, raw)
    signing = ['--sign', IDENTITY, '--timestamp']
    if args.keychain:
        signing += ['--keychain', args.keychain]
    run(['/usr/bin/codesign', '--force', *signing, '--identifier', IDENTIFIER, '--options', 'runtime', raw])
    run(['/usr/bin/codesign', '--verify', '--strict', '-R', REQUIREMENT, raw])
    report = {'version': version, 'architecture': slug, 'identity': IDENTITY, 'identifier': IDENTIFIER,
              'notarized': False,
              'designated_requirement': re.search(r'^designated => (.+)$', run(['/usr/bin/codesign', '-d', '-r-', raw]), re.MULTILINE)[1],
              'files': {raw.name: hashlib.sha256(raw.read_bytes()).hexdigest()}}
    (out / f'heart-portal-{slug}-verification.json').write_text(json.dumps(report, indent=2))
    print(f'Signed standalone binary: {raw}')
    print('Notarization: skipped.')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f'Build/signing failed: {error}', file=sys.stderr)
        sys.exit(1)
