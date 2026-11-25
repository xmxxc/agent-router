#!/usr/bin/env bash
set -euo pipefail

checkpoint=$(realpath -- "$1")
product=$2
target=$3
manifest=$4
tools=$(cd -- "$(dirname -- "$0")" && pwd)
export LC_ALL=C.UTF-8

cat /etc/os-release
uname -srm
id
[[ "$(id -u)" != 0 ]]
[[ "$(uname -m)" == x86_64 ]]
getconf GNU_LIBC_VERSION
dpkg-query -W libc6 libgcc-s1 libssl3 zlib1g jq

for executable in python python2 python3 pypy pypy2 pypy3 tmux herdr herdr-cli herdr-kernel herdr-kernel-prototype wfc-kernel; do
    if command -v "$executable"; then
        printf 'Unexpected executable %s\n' "$executable" >&2
        exit 1
    fi
done
dpkg-query -W -f='${db:Status-Status} ${Package}\n' | awk '
    $1 == "installed" && $2 ~ /^(python|libpython|pypy|tmux|herdr)/ { print; found = 1 }
    END { exit found }
'
executables=$(find /usr/bin /usr/local/bin /opt -regextype posix-extended -executable \
    -regex '.*/((python|pypy)([0-9]+(\.[0-9]+)*)?|tmux|herdr([-_][^/]*)?|wfc-kernel)')
if [[ -n "$executables" ]]; then
    printf 'Unexpected runtime files\n%s\n' "$executables" >&2
    exit 1
fi
for comm in /proc/[0-9]*/comm; do
    if { IFS= read -r process < "$comm"; } 2>/dev/null; then
        case "$process" in
            tmux*|herdr*) printf 'Unexpected running backend %s\n' "$process" >&2; exit 1 ;;
        esac
    fi
done
printf 'Runtime absence checks passed for Python, tmux and external Herdr.\n'

cd "$checkpoint"
printf '%s  %s\n' "$manifest" SHA256SUMS | sha256sum --check --strict
sha256sum --check --strict SHA256SUMS
work=$(mktemp -d /tmp/wfc-runtime.XXXXXXXX)
trap 'if [[ $? == 0 ]]; then rm -r -- "$work"; else printf "Private unpacked files retained at %s\n" "$work" >&2; fi' EXIT
archive="wfc-linux-x64-$product.tar.gz"
sha256sum "$archive"
stat --format='%n %s bytes' "$archive"
tar -xzf "$archive" -C "$work"
cd "$work"
sha256sum --check --strict BINARY-SHA256SUMS
read -r build_digest _ < "$checkpoint/binary.sha256"
printf '%s  %s\n' "$build_digest" wfc | sha256sum --check --strict
[[ "$(cat COMMIT.txt)" == "$product" ]]
sha256sum wfc
stat --format='%n %s bytes' wfc
dependencies=$(ldd ./wfc)
printf '%s\n' "$dependencies"
[[ "$dependencies" != *'not found'* ]]
version=$(./wfc --version)
printf '%s\n' "$version"
[[ "$version" == *"(Rust, $target, release, build $product)" ]]

/bin/bash "$tools/check_linux_bundled_package.sh" "$work/wfc"
