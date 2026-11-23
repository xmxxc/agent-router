#!/usr/bin/env bash
set -euo pipefail

product=$1
target=$2
manifest=$3
export LC_ALL=C.UTF-8

cat /etc/os-release
uname -srm
id
[[ "$(id -u)" != 0 ]]
[[ "$(uname -m)" == x86_64 ]]
getconf GNU_LIBC_VERSION
dpkg-query -W libc6 libgcc-s1 libssl3 zlib1g jq

for interpreter in python python2 python3 pypy pypy2 pypy3; do
    if command -v "$interpreter"; then
        printf 'Unexpected interpreter %s\n' "$interpreter" >&2
        exit 1
    fi
done
dpkg-query -W -f='${db:Status-Status} ${Package}\n' | awk '
    $1 == "installed" && $2 ~ /^(python|libpython|pypy)/ { print; found = 1 }
    END { exit found }
'
interpreters=$(find /usr/bin /usr/local/bin /opt -regextype posix-extended -type f -executable \
    -regex '.*/(python|pypy)([0-9]+(\.[0-9]+)*)?')
if [[ -n "$interpreters" ]]; then
    printf 'Unexpected interpreter files\n%s\n' "$interpreters" >&2
    exit 1
fi
printf 'No Python interpreter on PATH, in standard executable directories, or in installed packages.\n'

cd /checkpoint
printf '%s  %s\n' "$manifest" SHA256SUMS | sha256sum --check --strict
sha256sum --check --strict SHA256SUMS
work=$(mktemp -d /tmp/wfc-runtime.XXXXXXXX)
archive="wfc-linux-x64-$product.tar.gz"
sha256sum "$archive"
stat --format='%n %s bytes' "$archive"
tar -xzf "$archive" -C "$work"
cd "$work"
sha256sum --check --strict BINARY-SHA256SUMS
read -r build_digest _ < /checkpoint/binary.sha256
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

/bin/bash /check_linux_package.sh "$work/wfc"
