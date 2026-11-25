#!/usr/bin/env bash
# Run the synthetic fixture in the existing Jammy delegation scheme.
set -euo pipefail

if [[ "${1:-}" == in-delegation ]]; then
    uid=$2
    unit=$3
    scratch=$4
    binary=$5
    [[ "$(id -u)" == "$uid" && "$uid" -gt 0 ]]
    IFS= read -r membership < "/proc/$$/cgroup"
    [[ "$membership" == "0::/system.slice/$unit" ]]
    delegation="/sys/fs/cgroup${membership#0::}"
    test "$(stat -f -c '%T' "$delegation")" = cgroup2fs
    printf 'delegated_uid=%s unit=%s path=%s\n' "$uid" "$unit" "$delegation"
    exec /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TMPDIR="$scratch" \
        /usr/bin/python3 -Bu "$(dirname "$0")/probe.py" "$binary" "$delegation"
fi

verify_elf() {
    (cd "$EVIDENCE" && sha256sum --check --strict elf.sha256)
}

unit_missing() {
    [[ ! -e "/sys/fs/cgroup/system.slice/$unit" &&
        "$(systemctl show "$unit" --property=LoadState --value 2>/dev/null || :)" == not-found ]]
}

await_retirement() {
    local attempt
    for attempt in {1..50}; do
        if unit_missing; then
            return 0
        fi
        sleep .1
    done
    return 1
}

[[ "${GITHUB_RUN_ID:?}" =~ ^[0-9]+$ && "${GITHUB_RUN_ATTEMPT:?}" =~ ^[0-9]+$ ]]
unit="wfc-r29-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-migrated-reply.service"
unit_missing
verify_elf
scratch=$(mktemp -d "$RUNNER_TEMP/wfc-r29.XXXXXXXX")
finish() {
    local result=$? cleanup=0 identity=0
    trap - EXIT
    if ! await_retirement; then
        cleanup=1
        sudo -n systemctl stop "$unit" || :
        if await_retirement; then
            printf 'delegated_unit=%s retired=after-fallback\n' "$unit"
        else
            printf 'delegated_unit=%s retired=no\n' "$unit"
        fi
    else
        printf 'delegated_unit=%s retired=yes\n' "$unit"
    fi
    if ! rmdir -- "$scratch"; then
        cleanup=1
        rm -rf -- "$scratch" || :
    fi
    verify_elf || identity=$?
    printf 'fixture_exit=%s cleanup_exit=%s elf_check_exit=%s\n' "$result" "$cleanup" "$identity"
    if (( result != 0 )); then exit "$result"; fi
    if (( cleanup != 0 || identity != 0 )); then exit 1; fi
    printf 'BUNDLED_MIGRATED_ASK_REPLY_AND_CLEANUP_PASS\n'
}
trap finish EXIT

sudo -n systemd-run --unit="$unit" --wait --pipe --collect \
    --property=Type=exec --property="User=$(id -un)" --property=Delegate=yes \
    --property="WorkingDirectory=$PWD" --property=RuntimeMaxSec=120s --property=TimeoutStopSec=10s \
    /bin/bash "$(realpath "$0")" in-delegation "$(id -u)" "$unit" "$scratch" "$EVIDENCE/wfc" \
    |& tee "$EVIDENCE/migrated-ask-reply.log"
test "$(grep -Fxc BUNDLED_MIGRATED_ASK_REPLY_PASS "$EVIDENCE/migrated-ask-reply.log")" = 1
