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
    097d71073d5d5e387753c6a1de2a6cb0acce6cb7bc6c46c1306422a649e81e48 "$fixture" |
    sha256sum --check --strict
mkdir -- "$scratch/tools"
patched="$scratch/$fixture"
cp -- "$fixture" "$patched"
# The signed reply base inserted 56 lines before the approved callback method.
# Relocate only the zero-context hunk coordinates in the temporary stream.
sed -e 's/^@@ -562,3 +562,23 @@$/@@ -618,3 +618,23 @@/' \
    -e 's/^@@ -567 +587,2 @@$/@@ -623 +643,2 @@/' \
    -e 's/^@@ -570,0 +592,4 @@$/@@ -626,0 +648,4 @@/' "$overlay" |
    git -C "$scratch" apply --no-index --unidiff-zero --include="$fixture" -
printf '%s  %s\n' c511cee8bce23984074b438d81b885b6fb8f502b5b2054dd82bfdbfb9b76f187 "$patched" |
    sha256sum --check --strict
printf 'fixtureBlob=%s\n' "$(git hash-object "$patched")"
sha256sum "$overlay" "$patched"

python3 -Bu "$patched" "$@"
