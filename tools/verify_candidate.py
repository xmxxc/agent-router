"""Check a downloaded candidate against its GitHub release and the source checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import tarfile
import tempfile
import zipfile

from collect_notices import UUCODE

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    'linux-x64': ('tar.gz', 'x86_64-unknown-linux-gnu'),
    'windows-x64': ('zip', 'x86_64-pc-windows-msvc'),
}


def digest(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def check_notices(notices, target):
    index = json.loads((notices / 'sources.json').read_bytes())
    if index['target'] != target:
        raise ValueError(f'Wrong notices target for {target}')
    files = {
        'Cargo.lock': 'Cargo.lock',
        'LICENSE-Ghostty': 'licenses/LICENSE-Ghostty',
        'LICENSE-Herdr': 'licenses/LICENSE-Herdr',
        'simdutf/LICENSE-simdutf-MIT': 'licenses/LICENSE-simdutf-MIT',
        'simdutf/NOTICE-simdutf-isadetection': 'licenses/NOTICE-simdutf-isadetection',
        'xorg-rgb/COPYING': 'licenses/xorg-rgb/COPYING',
        'provenance/SOURCES.md': 'SOURCES.md',
        'provenance/vendor/INPUTS.sha256': 'vendor/INPUTS.sha256',
    }
    wfc = next(p for p in index['cargo'] if p['name'] == 'wfc')
    files[f"cargo/wfc-{wfc['version']}/LICENSE-WFC"] = 'LICENSE-WFC'
    for license_name in ('LICENSE_Bjoern_Hoehrmann', 'LICENSE_unicode'):
        files[f'zig/{UUCODE}/licenses/{license_name}'] = f'licenses/uucode/licenses/{license_name}'
    for name, source in files.items():
        if (notices / name).read_bytes() != (ROOT / source).read_bytes():
            raise ValueError(f'Packaged {name} differs from {source}')
    for package in index['cargo']:
        directory = notices / 'cargo' / f"{package['name']}-{package['version']}"
        if not package['notices']:
            raise ValueError(f'Missing notices for {directory.name}')
        for name in package['notices']:
            if not (directory / name).stat().st_size:
                raise ValueError(f'Empty notice in {directory.name}: {name}')
    for package in index['zig_materialized']:
        for name in package['notices']:
            if not (notices / 'zig' / package['package_hash'] / name).stat().st_size:
                raise ValueError(f'Empty Zig notice: {name}')


def verify(downloads, tag, commit, platform, output):
    extension, target = TARGETS[platform]
    archive = downloads / f'wfc-{platform}-{commit}.{extension}'
    manifest = downloads / 'SHA256SUMS'
    release = json.loads((downloads / 'release.json').read_bytes())
    if release['tag_name'] != tag or release['draft'] or not release['prerelease']:
        raise ValueError(f'Expected the published prerelease {tag}')
    assets = {asset['name']: asset for asset in release['assets']}
    archive_hash = digest(archive)
    for path, checksum in ((archive, archive_hash), (manifest, digest(manifest))):
        asset = assets[path.name]
        size = path.stat().st_size
        if asset['digest'] != f'sha256:{checksum}' or asset['size'] != size:
            raise ValueError(f'GitHub digest or size differs for {path.name}')
        print(f'{path.name}  {size} bytes  sha256:{checksum}', flush=True)
    entries = [line.split(maxsplit=1) for line in manifest.read_text().splitlines()]
    if [h for h, n in entries if n.removeprefix('*') == archive.name] != [archive_hash]:
        raise ValueError(f'{archive.name} differs from SHA256SUMS')

    if output.exists():
        raise FileExistsError(output)
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        stage = Path(tmp) / 'candidate'
        if extension == 'zip':
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(stage)
            if digest(stage / 'wfc.exe') != digest(stage / 'wfc-kernel.exe'):
                raise ValueError('Windows CLI and companion differ')
        else:
            with tarfile.open(archive) as bundle:
                bundle.extractall(stage, filter='data')
        if (stage / 'COMMIT.txt').read_text().strip() != commit:
            raise ValueError(f'Expected commit {commit} inside {archive.name}')
        check_notices(stage / 'bundled-notices', target)
        stage.rename(output)
    print(f'Checked {target}, commit {commit}, bundled notices in {output}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('downloads', type=Path)
    parser.add_argument('tag')
    parser.add_argument('commit')
    parser.add_argument('platform', choices=TARGETS)
    parser.add_argument('output', type=Path, help='New directory for the verified package')
    verify(**vars(parser.parse_args()))
