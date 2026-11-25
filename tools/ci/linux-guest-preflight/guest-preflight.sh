#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C.UTF-8

cat /etc/os-release
uname -srm
id
[[ "$(id -u)" == 1000 && "$(uname -m)" == x86_64 ]]
grep -Eq '(^| )wfc_guest_preflight=1( |$)' /proc/cmdline
IFS= read -r membership < "/proc/$$/cgroup"
[[ "$membership" == 0::/system.slice/wfc-guest-preflight.service ]]
root="/sys/fs/cgroup${membership#0::}"
[[ "$(stat -f -c %T -- "$root")" == cgroup2fs && -w "$root" && -w "$root/cgroup.procs" && -f "$root/cgroup.kill" ]]
for tool in bash awk cmp find grep ldd nc script sha256sum stat tar gzip timeout realpath dpkg-query getconf; do
    command -v "$tool"
done
getconf GNU_LIBC_VERSION
dpkg-query -W bash coreutils findutils diffutils tar gzip libc6 libgcc-s1 libssl3 zlib1g jq systemd
jq --version
for executable in python python2 python3 pypy pypy2 pypy3 tmux herdr herdr-cli herdr-kernel herdr-kernel-prototype wfc wfc-kernel; do
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
    -regex '.*/((python|pypy)([0-9]+(\.[0-9]+)*)?|tmux|herdr([-_][^/]*)?|wfc(-kernel)?)')
[[ -z "$executables" ]]
printf 'Guest installation and executable checks passed.\n'

script -q -e -c 'test -t 0 && test -t 1 && printf "GUEST_PTY_PASS\n"' /dev/null </dev/null
port=49187
printf 'guest-loopback\n' | timeout 5s nc -l -N 127.0.0.1 "$port" &
listener=$!
response=''
for _ in {1..30}; do
    if response=$(timeout 0.5s nc -w 1 127.0.0.1 "$port"); then
        break
    fi
    sleep 0.1
done
wait "$listener"
[[ "$response" == guest-loopback ]]
printf 'GUEST_LOOPBACK_PASS\n'

child=$(mktemp -d "$root/guest-probe.XXXXXXXX")
trap 'rmdir -- "$child" || true' EXIT
[[ -w "$child/cgroup.procs" && -w "$child/cgroup.kill" ]]
(
    printf '0\n' > "$child/cgroup.procs"
    exec /bin/sleep 15
) &
worker=$!
for _ in {1..40}; do
    if grep -Fxq "$worker" "$child/cgroup.procs"; then
        break
    fi
    sleep 0.05
done
grep -Fxq "$worker" "$child/cgroup.procs"
[[ "$(cat "/proc/$worker/cgroup")" == "0::${child#/sys/fs/cgroup}" ]]
printf '1\n' > "$child/cgroup.kill"
rc=0
wait "$worker" || rc=$?
test "$rc" -eq 137
grep -Fx 'populated 0' "$child/cgroup.events"
rmdir -- "$child"
trap - EXIT
[[ ! -e "$child" && ! -e "/proc/$worker" ]]
printf 'GUEST_DELEGATION_PASS uid=%s pid=%s\n' "$(id -u)" "$worker"
printf 'WFC_CLEAN_GUEST_PREFLIGHT_PASS\n'
