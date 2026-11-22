"""Run the six native WFC lifecycle contracts against a built companion."""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import unittest

WFC = '3c06c0905faaf9e61de49404c33e70720409607f'
TESTS = [
    'standalone_dispatch_reply_done_and_close',
    'wrong_nonce_and_generation_cannot_input_or_close',
    'interrupt_close_resume_replaces_execution_identity',
    'close_contains_setsid_descendant',
    'parent_first_exit_cleans_descendant_and_allows_resume',
    'runtime_forced_death_cleans_descendants_without_restarting',
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('checkout', type=Path, help='WFC integration-test checkout')
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    root, binary = args.checkout.resolve(), args.binary.resolve(strict=True)
    if subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip() != WFC:
        parser.error(f'WFC checkout must be at {WFC}')
    subprocess.run(['git', '-C', str(root), 'diff', '--quiet', 'HEAD', '--', '*.py'], check=True)
    if sys.platform.startswith('linux') and not os.environ.get('WFC_BUNDLED_CGROUP_ROOT'):
        parser.error('Set WFC_BUNDLED_CGROUP_ROOT to a delegated cgroup v2 directory')
    os.environ['WFC_BUNDLED_TEST_KERNEL'] = str(binary)
    os.environ.pop('WFC_BUNDLED_TEST_DISCOVERY', None)
    os.environ.pop('WFC_TEST_PROGRAM', None)
    sys.dont_write_bytecode = True
    os.environ['PYTHONDONTWRITEBYTECODE'] = '1'
    sys.path[:0] = [str(root), str(root / 'tests')]
    print(f'WFC {WFC}, kernel {binary}, SHA256 {hashlib.sha256(binary.read_bytes()).hexdigest()}', flush=True)
    names = ['test_bundled.BundledNative.test_' + name for name in TESTS]
    suite = unittest.defaultTestLoader.loadTestsFromNames(names)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return result.wasSuccessful() and result.testsRun == len(TESTS) and not result.skipped


if __name__ == '__main__':
    sys.exit(0 if main() else 1)
