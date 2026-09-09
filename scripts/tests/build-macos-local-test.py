#!/usr/bin/env python3
"""Build two real standalone signed releases for local upgrade validation.

The source copies differ only in version. No installer wrappers, DMGs, patched
verification policy or test-specific runtime are introduced.
"""
import argparse
import json
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import hashlib
import urllib.request

REPO = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=REPO / 'dist/macos-user-test')
    parser.add_argument('--baseline-tag', default='v0.8.0', help='An actual published GitHub release; never compiled locally.')
    parser.add_argument('--release-metadata', type=Path, help='Use previously fetched GitHub release metadata (avoids API rate limits).')
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('Build on macOS with the Developer ID certificate.')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    old = re.search(r'^version = "([^"]+)"', (REPO / 'portal/Cargo.toml').read_text(), re.MULTILINE)[1]
    major, minor, patch = map(int, old.split('.'))
    new = f'{major}.{minor}.{patch + 1}'
    slug = 'macos-arm64' if platform.machine() == 'arm64' else 'macos-x86_64'
    request = urllib.request.Request('https://api.github.com/repos/d5z/heart-portal/releases/tags/' + args.baseline_tag,
                                     headers={'User-Agent': 'heart-portal-local-validation'})
    release = (json.loads(args.release_metadata.read_text()) if args.release_metadata
               else json.load(urllib.request.urlopen(request, timeout=30)))
    if release['tag_name'] != args.baseline_tag:
        raise RuntimeError('Cached GitHub metadata does not match the requested baseline tag.')
    asset = next(a for a in release['assets'] if a['name'] == f'heart-portal-{slug}')
    downloaded = urllib.request.urlopen(asset['browser_download_url'], timeout=60).read()
    digest = 'sha256:' + hashlib.sha256(downloaded).hexdigest()
    if not asset.get('digest') or digest != asset['digest']:
        raise RuntimeError('Published baseline must match GitHub\'s asset digest.')
    baseline_version = release['tag_name'].removeprefix('v')
    baseline = out / baseline_version / asset['name']
    baseline.parent.mkdir(parents=True, exist_ok=True)
    baseline.write_bytes(downloaded)
    baseline.chmod(0o755)
    subprocess.run(['/usr/bin/codesign', '--verify', '--strict', '-R',
                    '=anchor apple generic and identifier "com.aspect.heart-portal" and certificate leaf[subject.OU] = "7N8XHQWCNN"',
                    str(baseline)], check=True)
    (baseline.parent / 'github-provenance.json').write_text(json.dumps({
        'source': 'unaltered GitHub release asset', 'tag': release['tag_name'], 'url': asset['browser_download_url'],
        'release_url': release['html_url'], 'sha256': digest.split(':', 1)[1], 'size': len(downloaded),
        'locally_compiled': False, 'locally_resigned': False}, indent=2))
    build = REPO / 'target/portal-macos-local-build'
    (REPO / 'target').mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='macos-user-source-', dir=REPO / 'target') as temporary:
        fixture = Path(temporary)
        for name in ('Cargo.toml', 'Cargo.lock', 'portal.example.toml', 'LICENSE'):
            shutil.copy2(REPO / name, fixture)
        shutil.copytree(REPO / 'portal', fixture / 'portal', ignore=shutil.ignore_patterns('target'))
        shutil.copytree(REPO / 'scripts', fixture / 'scripts', ignore=shutil.ignore_patterns('__pycache__'))
        for version in (old, new):
            if version == new:
                for path, before, after in (
                    (fixture / 'portal/Cargo.toml', f'version = "{old}"', f'version = "{new}"'),
                    (fixture / 'Cargo.lock', f'name = "heart-portal"\nversion = "{old}"', f'name = "heart-portal"\nversion = "{new}"')):
                    content = path.read_text()
                    if content.count(before) != 1:
                        raise RuntimeError('Fixture version substitution is ambiguous.')
                    path.write_text(content.replace(before, after, 1))
            print(f'Building actual standalone binary {version}', flush=True)
            subprocess.run(['cargo', 'build', '-p', 'heart-portal', '--bin', 'heart-portal', '--release', '--locked',
                '--manifest-path', str(fixture / 'Cargo.toml'), '--target-dir', str(build)], check=True)
            subprocess.run([sys.executable, str(fixture / 'scripts/package-portal-macos.py'),
                '--binary', str(build / 'release/heart-portal'), '--output', str(out / version)], check=True)
    shutil.copy2(REPO / 'portal.example.toml', out / 'portal.example.toml')
    (out / 'versions.json').write_text(json.dumps({'initial': baseline_version, 'first_upgrade': old, 'candidate': new,
        'binary_name': f'heart-portal-{slug}', 'notarized': False,
        'source_difference': 'Initial version is the original GitHub release. Local first_upgrade/candidate differ only in Cargo version.',
        'artificial_test_version': new}, indent=2))
    print(f'Ready: {out}. No service was installed or started.')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Build failed: {error}', file=sys.stderr)
        sys.exit(1)
