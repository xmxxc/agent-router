"""Materialize the pinned Ghostty Zig sources and prime the WFC zlib input."""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
ARCHIVE = ROOT / 'vendor/libghostty-vt-source.zip'
ARCHIVE_SHA256 = '8631810d25448eadc598350b9522aa8622c1c7d356935249ab88b5a13bc5ba6d'
ZLIB_URL = 'https://deps.files.ghostty.org/zlib-1220fed0c74e1019b3ee29edae2051788b080cd96e90d56836eea857b0b966742efb.tar.gz'
ZLIB_SHA256 = '17e88863f3600672ab49182f217281b6fc4d3c762bde361935e436a95214d05c'
ZLIB_PACKAGE = 'N-V-__8AAB0eQwD-0MdOEBmz7intriBReIsIDNlukNVoNu6o'


def materialize(dest):
    if hashlib.sha256(ARCHIVE.read_bytes()).hexdigest() != ARCHIVE_SHA256:
        raise ValueError('Ghostty source ZIP checksum mismatch')
    with zipfile.ZipFile(ARCHIVE) as source:
        if not dest.exists():
            with tempfile.TemporaryDirectory(dir=dest.parent) as tmp:
                stage = Path(tmp) / 'source'
                source.extractall(stage)
                try:
                    stage.rename(dest)
                except OSError:
                    if not dest.is_dir():
                        raise

        # Keep Zig's package/build caches across runs. Check every source byte
        # and reject unexpected files that could shadow a pinned input.
        if dest.is_symlink():
            raise ValueError(f'Expected a local Ghostty directory: {dest}')
        expected = {i.filename for i in source.infolist() if not i.is_dir()}
        for directory, dirs, files in os.walk(dest):
            root = Path(directory)
            if root == dest:
                dirs[:] = [d for d in dirs if d not in {'.zig-cache', 'zig-out', 'zig-pkg'}]
            for name in dirs:
                if (root / name).is_symlink():
                    raise ValueError(f'Unexpected source symlink: {root / name}')
            for name in files:
                path = root / name
                rel = path.relative_to(dest).as_posix()
                if rel not in expected or path.is_symlink() or path.read_bytes() != source.read(rel):
                    raise ValueError(f'Unexpected or modified Ghostty source: {path}')
                expected.remove(rel)
        if expected:
            raise ValueError(f'Missing Ghostty source: {min(expected)}')


def prime_zlib(zig, dest, archive):
    if hashlib.sha256(archive.read_bytes()).hexdigest() != ZLIB_SHA256:
        raise ValueError('zlib archive checksum mismatch')
    package = subprocess.check_output([zig, 'fetch', str(archive.resolve())], cwd=dest, text=True).strip()
    if package != ZLIB_PACKAGE:
        raise ValueError(f'Unexpected zlib Zig package hash: {package}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--zlib-archive', type=Path, help='Use this cached source archive')
    args = parser.parse_args()
    zig = os.environ.get('ZIG', 'zig')
    if subprocess.check_output([zig, 'version'], text=True).strip() != '0.16.0':
        parser.error('Zig 0.16.0 is required')
    dest = ROOT / 'vendor/libghostty-vt'
    materialize(dest)
    if args.zlib_archive:
        prime_zlib(zig, dest, args.zlib_archive)
    else:
        with tempfile.TemporaryDirectory() as tmp:
            archive = Path(tmp) / 'zlib.tar.gz'
            with urllib.request.urlopen(ZLIB_URL, timeout=60) as response:
                archive.write_bytes(response.read())
            prime_zlib(zig, dest, archive)
    print(f'Ghostty source and zlib verified ({ARCHIVE_SHA256}, {ZLIB_PACKAGE})')


if __name__ == '__main__':
    main()
