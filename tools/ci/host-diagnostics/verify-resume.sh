#!/bin/bash
# Run only the selected resume tests, using the independently reviewed fixture patch.
set -euo pipefail
fixture=tools/check_tmux.py
overlay=$(realpath -- "$(dirname -- "$0")/callback-fixture.patch")
scratch=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/wfc-resume.XXXXXX")
cleanup() {
    if [[ -f "$scratch/$fixture" ]]; then
        unlink -- "$scratch/$fixture"
    fi
    if [[ -d "$scratch/tools" ]]; then
        rmdir -- "$scratch/tools"
    fi
    rmdir -- "$scratch"
}
trap cleanup EXIT

printf 'resumeFixtureSource=%s\n' 7441bf7c38eeb6aba6d2c7ad71cd6c9640d8ac42
printf '%s  %s\n' \
    3352bcc9d9784cb99adb975f0c04d7a5a21bd6d9570cf8f9f3e8f706771af6d8 "$overlay" \
    6417c72f66aa5bc88fe828f1c011fe043d2fd2b13f112473b5e46d5c5ce5aebe "$fixture" |
    sha256sum --check --strict
mkdir -- "$scratch/tools"
patched="$scratch/$fixture"
cp -- "$fixture" "$patched"
git -C "$scratch" apply --no-index --unidiff-zero --include="$fixture" "$overlay"
printf '%s  %s\n' 4c4e46cd7afdea99641454eb316400ba23bbddf464f7c3dcb57af7312a6f1116 "$patched" |
    sha256sum --check --strict
printf 'fixtureBlob=%s\n' "$(git hash-object "$patched")"
sha256sum "$overlay" "$patched"

python3 -Bu "$patched" "$@"
