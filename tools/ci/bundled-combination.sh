#!/usr/bin/env bash
# Drive the fixed product's existing fixtures from its checkout.
set -euo pipefail

if [[ $1 == codex-fixtures ]]; then
    binary=$2
    # Keep the JSON stdout separate from the unfinished unittest stderr line.
    receipt=$(/usr/bin/python3 -Bu tests/check_codex.py "$binary" -v Checker.test_original_done_ack_and_close)
    printf '%s\n' "$receipt"
    exec /usr/bin/python3 -Bu tests/startup_failure.py "$binary" -v \
        StartupFailure.test_script_lookup_permission_failure
fi

if [[ $1 == in-delegation ]]; then
    shift
    uid=$1
    unit=$2
    tmp=$3
    binary=$4
    mode=$5
    [[ "$(id -u)" == "$uid" && "$uid" -gt 0 ]]
    IFS= read -r membership < "/proc/$$/cgroup"
    [[ "$membership" == "0::/system.slice/$unit" ]]
    delegation="/sys/fs/cgroup${membership#0::}"
    test "$(stat -f -c '%T' "$delegation")" = cgroup2fs
    printf 'delegated_uid=%s unit=%s path=%s\n' "$uid" "$unit" "$delegation"
    if [[ "$mode" == codex-smoke ]]; then
        exec /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TMPDIR="$tmp" \
            WFC_BUNDLED_CGROUP_ROOT="$delegation" /bin/bash "$0" codex-fixtures "$binary"
    fi
    exec /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TMPDIR="$tmp" \
        WFC_TEST_DELEGATION="$delegation" /bin/sh tools/check_bundled_default.sh "$binary" "$mode"
fi

group=$1
binary="$EVIDENCE/wfc"
scratch=$(mktemp -d "$RUNNER_TEMP/wfc-r46-$group.XXXXXXXX")
tmp="$scratch/fixture"
transcript="$scratch/fixture.log"
delegated_unit=
strict_cleanup=no

verify_elf() {
    (cd "$EVIDENCE" && sha256sum --check --strict elf.sha256)
}

unit_missing() {
    [[ ! -e "/sys/fs/cgroup/system.slice/$1" &&
        "$(systemctl show "$1" --property=LoadState --value 2>/dev/null || :)" == not-found ]]
}

check_delegated_cleanup() {
    local unit=$1
    for _ in {1..50}; do
        if unit_missing "$unit"; then
            printf 'delegated_unit=%s retired=yes\n' "$unit"
            return 0
        fi
        sleep .1
    done
    printf 'delegated_unit=%s retired=no\n' "$unit"
    sudo -n systemctl stop "$unit" || :
    return 1
}

finish() {
    local result=$?
    local cleanup=0 identity=0
    trap - EXIT
    verify_elf || identity=$?
    if [[ -n "$delegated_unit" ]]; then
        check_delegated_cleanup "$delegated_unit" || cleanup=$?
    fi
    # The shell fixture leaves files for inspection by its caller. Python
    # fixtures manage their own temporary directories and must leave none.
    if [[ "$strict_cleanup" == yes && -d "$tmp" ]]; then
        rmdir -- "$tmp" || cleanup=1
    fi
    rm -rf -- "$scratch" || cleanup=1
    printf 'group=%s fixture_exit=%s cleanup_exit=%s elf_check_exit=%s\n' "$group" "$result" "$cleanup" "$identity"
    if (( result != 0 )); then exit "$result"; fi
    if (( cleanup != 0 || identity != 0 )); then exit 1; fi
    exit 0
}
trap finish EXIT
mkdir "$tmp"
verify_elf

clean_run() {
    /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TMPDIR="$tmp" "$@"
}

bundled_marker() {
    local marker
    case "$1" in
        ordinary) marker=BUNDLED_DEFAULT_ORDINARY_PASS ;;
        user-service) marker=BUNDLED_USER_SERVICE_PASS ;;
        delegated) marker=BUNDLED_DEFAULT_DELEGATED_PASS ;;
        continue) marker=BUNDLED_CONTINUE_RECEIPT_PASS ;;
        nudge) marker=BUNDLED_NUDGE_RECEIPT_PASS ;;
        reply) marker=BUNDLED_REPLY_RECEIPT_PASS ;;
        *) return 2 ;;
    esac
    grep -Fx "$marker" "$transcript"
    if [[ "$1" == reply ]]; then
        test "$(grep -Ec '^BUNDLED_REPLY_PASTE_ENTER_NS=[0-9]+$' "$transcript")" = 1
    fi
}

codex_markers() {
    local mode=$1 digest filename
    read -r digest filename < "$EVIDENCE/elf.sha256"
    [[ "$digest" =~ ^[0-9a-f]{64}$ && "$filename" == wfc ]]
    test "$(grep -c '^BUNDLED_CODEX_RECEIPT ' "$transcript")" = 1
    grep -F "BUNDLED_CODEX_RECEIPT {\"mode\":\"$mode\",\"binarySha256\":\"$digest\"," "$transcript"
    grep -E '^test_original_done_ack_and_close .* \.\.\. ok$' "$transcript"
    grep -E '^test_script_lookup_permission_failure .* \.\.\. ok$' "$transcript"
    test "$(grep -Fc 'Ran 1 test in ' "$transcript")" = 2
    test "$(grep -Fxc OK "$transcript")" = 2
    printf 'synthetic_codex_smoke=%s verified=yes\n' "$group"
}

case "$group" in
    user-codex-smoke)
        strict_cleanup=yes
        clean_run /bin/bash "$0" codex-fixtures "$binary" |& tee "$transcript"
        codex_markers user-service
        ;;
    ordinary|user-service|user-continue|user-nudge|user-reply)
        mode=${group#user-}
        if [[ "$group" == user-service ]]; then mode=user-service; fi
        clean_run /bin/sh tools/check_bundled_default.sh "$binary" "$mode" |& tee "$transcript"
        bundled_marker "$mode"
        if [[ "$mode" == ordinary ]]; then
            if grep -Fx BUNDLED_NO_USER_MANAGER_PASS "$transcript"; then
                printf 'ordinary_no_user_manager=verified\n'
            else
                printf 'ordinary_no_user_manager=not-exercised\n'
            fi
        fi
        ;;
    delegated|delegated-continue|delegated-nudge|delegated-reply|delegated-codex-smoke)
        mode=${group#delegated-}
        [[ "${GITHUB_RUN_ID:?}" =~ ^[0-9]+$ && "${GITHUB_RUN_ATTEMPT:?}" =~ ^[0-9]+$ ]]
        unit="wfc-r46-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$mode.service"
        unit_missing "$unit"
        delegated_unit=$unit
        if [[ "$mode" == codex-smoke ]]; then strict_cleanup=yes; fi
        sudo -n systemd-run --unit="$unit" --wait --pipe --collect \
            --property=Type=exec --property="User=$(id -un)" --property=Delegate=yes \
            --property="WorkingDirectory=$PWD" --property=RuntimeMaxSec=120s --property=TimeoutStopSec=10s \
            /bin/bash "$(realpath "$0")" in-delegation "$(id -u)" "$unit" "$tmp" "$binary" "$mode" \
            |& tee "$transcript"
        if [[ "$mode" == codex-smoke ]]; then
            codex_markers explicit
        else
            bundled_marker "$mode"
        fi
        printf 'explicit_delegation=%s verified=yes\n' "$mode"
        ;;
    startup)
        strict_cleanup=yes
        clean_run /usr/bin/python3 -Bu tools/check_bundled_startup.py "$binary" |& tee "$transcript"
        grep -E '^BUNDLED_STARTUP_COMMITTED_AFTER_OUTPUT_LOSS d-[0-9a-f]+$' "$transcript"
        grep -E '^BUNDLED_STARTUP_INTERRUPTION_PASS d-[0-9a-f]+ a-[0-9a-f]+ wfc-bundled-[0-9a-f-]+\.service$' "$transcript"
        ;;
    archive)
        strict_cleanup=yes
        clean_run /usr/bin/python3 -Bu tests/legacy_tmux_archive.py "$binary" |& tee "$transcript"
        grep -Fx 'native: explicit and inferred history, preserved receipts and no execution target' "$transcript"
        grep -Fx 'mixed: retired attempts and damaged reservations preserved, active receipts delivered' "$transcript"
        grep -Fx 'publication: inconsistent generations left untouched, matching callbacks delivered' "$transcript"
        grep -Fx 'SQLite-only: history visible, live state unknown, original bytes preserved' "$transcript"
        ;;
    write-fence)
        strict_cleanup=yes
        clean_run /usr/bin/python3 -Bu tests/bundled_write_fence.py "$binary" |& tee "$transcript"
        test "$(grep -c '^BUNDLED_WRITE_FENCE_PASS ' "$transcript")" = 2
        ;;
    owner)
        strict_cleanup=yes
        clean_run /usr/bin/python3 -Bu tests/owner.py "$binary" \
            test_controlled_startup_failure_keeps_owner_notice_and_ack |& tee "$transcript"
        grep -F 'Ran 1 test in ' "$transcript"
        grep -Fx OK "$transcript"
        ;;
    herdr)
        strict_cleanup=yes
        herdr="$scratch/herdr"
        curl --fail --silent --show-error --location --retry 3 --max-time 120 \
            https://github.com/herdrdev/herdr/releases/download/v0.9.5/herdr-linux-x86_64 -o "$herdr"
        herdr_sha=14d9e8bc8b4a9e0118d77c32266f76d461f09b40342eb23752960bd89f59929d
        printf '%s  %s\n' "$herdr_sha" "$herdr" | sha256sum --check --strict
        chmod 700 "$herdr"
        test "$("$herdr" --version)" = 'herdr 0.9.5'
        printf 'herdrVersion=0.9.5 herdrSha256=%s\n' "$herdr_sha"
        result=0
        for selector in test_reclaim_uses_verified_herdr_close_and_keeps_sibling test_public_reply_on_bound_herdr_pane; do
            verify_elf
            status=0
            # unittest writes the selector and result to stderr around the
            # fixture's stdout, which can contain a receipt printed mid-test.
            clean_run /usr/bin/python3 -Bu tests/herdr_public.py "$herdr" "$binary" -v \
                "PublicLifecycle.$selector" 2>"$transcript" || status=$?
            cat "$transcript"
            if (( status == 0 )) &&
                grep -E "^$selector .* \.\.\. ok$" "$transcript" &&
                grep -F 'Ran 1 test in ' "$transcript" && grep -Fx OK "$transcript"; then
                printf 'herdr_selector=%s verified=yes\n' "$selector"
            else
                printf 'herdr_selector=%s verified=no fixture_exit=%s\n' "$selector" "$status"
                result=1
            fi
            verify_elf
            printf '%s  %s\n' "$herdr_sha" "$herdr" | sha256sum --check --strict
        done
        exit "$result"
        ;;
    *) exit 2 ;;
esac
