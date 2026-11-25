#!/usr/bin/env bash
# Run the frozen product fixtures against the one downloaded ELF.
set -euo pipefail
umask 077

binary=$(realpath -e -- "$1")
expected=$2
handoff_recipe=$(realpath -e -- "$3")
product=$4
resume_fixture=$(realpath -e -- "$5")
[[ "$expected" =~ ^[0-9a-f]{64}$ && "$product" =~ ^[0-9a-f]{40}$ &&
   "${binary##*/}" == wfc && "$(id -u)" -gt 0 ]]
package=$(dirname -- "$binary")
scratch=$(mktemp -d "${RUNNER_TEMP:?}/wfc-candidate-hosted.XXXXXXXX")
trap 'if (( $? == 0 )); then rm -r -- "$scratch"; else printf "HOSTED_PRIVATE_DIAGNOSTICS_RETAINED\n" >&2; fi' EXIT

verify_elf() {
  printf '%s  %s\n' "$expected" "$binary" | sha256sum --check --status
}

verify_elf
test "$(cat "$package/COMMIT.txt")" = "$product"
reference=$(env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 "$binary" --version)
[[ "$reference" == *"(Rust, x86_64-unknown-linux-gnu, release, build $product)" ]]
printf 'release_version_verified=%s\n' "$product"
getconf GNU_LIBC_VERSION
ldd "$binary" > "$scratch/ldd.log"
if grep -Fq 'not found' "$scratch/ldd.log"; then
  printf 'DOWNLOAD_ELF_LIBRARY_UNAVAILABLE\n' >&2
  exit 1
fi
printf '%s  wfc\n' "$expected" > "$package/elf.sha256"
runtime="/run/user/$(id -u)"
if [[ ! -S "$runtime/bus" ]] ||
   ! env -i PATH=/usr/bin:/bin XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus" \
      /usr/bin/systemctl --user --no-pager show --property=Version --value >/dev/null 2>&1; then
  printf 'HOSTED_USER_MANAGER_BLOCKED\n' >&2
  exit 78
fi
printf 'hosted_uid=%s mode=default-user-service elf_sha256=%s\n' "$(id -u)" "$expected"

run_fixture() {
  local name=$1 fixture_dir rc=0
  shift
  fixture_dir="$scratch/$name"
  mkdir -- "$fixture_dir"
  transcript="$scratch/$name.log"
  env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TMPDIR="$fixture_dir" \
    RUNNER_TEMP="$fixture_dir" EVIDENCE="$package" "$@" > "$transcript" 2>&1 || rc=$?
  verify_elf
  if ((rc != 0)); then
    if [[ "$name" == public-resume ]]; then
      grep -Ex 'BUNDLED_RESUME_FIXTURE_FAILED (prepare|manager|dispatch|source-stop|resume|done-ack|replacement-close)|BUNDLED_RESUME_CLEANUP_UNCONFIRMED' "$transcript" || true
    fi
    printf 'fixture=%s exit=%s elf_verified=yes\n' "$name" "$rc" >&2
    return "$rc"
  fi
  if ! rmdir -- "$fixture_dir"; then
    printf 'fixture=%s private_cleanup=unconfirmed\n' "$name" >&2
    return 1
  fi
}

emit_once() {
  local match
  # The pinned wrapper repeats confirmed markers in its own validation output.
  match=$(grep -Ex -- "$1" "$transcript" | sort -u)
  [[ -n "$match" && "$match" != *$'\n'* ]]
  printf '%s\n' "$match"
}

sha='[0-9a-f]{64}'
run_fixture public-resume /usr/bin/python3 -Bu "$resume_fixture" "$binary"
emit_once "BUNDLED_RESUME_SOURCE_STOPPED $sha $sha $sha"
emit_once "BUNDLED_RESUME_REPLACEMENT_ONCE $sha $sha"
emit_once 'BUNDLED_RESUME_DONE_ACK [1-9][0-9]* 3'
emit_once "BUNDLED_RESUME_REPLACEMENT_CLOSED $sha $sha"
emit_once 'BUNDLED_RESUME_PUBLIC_PASS'

run_fixture user-handoff /bin/bash "$handoff_recipe" user-handoff
emit_once "BUNDLED_CODEX_OPENCODE_HANDOFF_PASS round=1 ticket_sha256=$sha"
emit_once 'handoff_close_evidence=verified initial=(uncertain|completed) terminal=(closed|exited) reads=[1-9][0-9]*'
emit_once 'group=user-handoff fixture_exit=0 cleanup_exit=0 elf_check_exit=0'

run_fixture user-service /bin/bash "$handoff_recipe" user-service
emit_once 'BUNDLED_USER_SERVICE_PASS'
emit_once 'group=user-service fixture_exit=0 cleanup_exit=0 elf_check_exit=0'

run_fixture startup /bin/bash "$handoff_recipe" startup
emit_once 'BUNDLED_STARTUP_COMMITTED_AFTER_OUTPUT_LOSS d-[0-9a-f]{32}'
emit_once 'BUNDLED_STARTUP_INTERRUPTION_PASS d-[0-9a-f]{32} a-[0-9a-f]{32} wfc-bundled-[0-9a-f-]{36}\.service'
emit_once 'group=startup fixture_exit=0 cleanup_exit=0 elf_check_exit=0'

run_fixture user-codex-smoke /bin/bash "$handoff_recipe" user-codex-smoke
sed -n 's/^BUNDLED_CODEX_RECEIPT //p' "$transcript" | sort -u | jq -ers --arg elf "$expected" '
  if length != 1 then error("expected one unique Codex receipt") else .[0] end
  | select(.mode == "user-service" and .binarySha256 == $elf and .round == 1
      and (.dispatch | test("^d-[0-9a-f]{32}$")) and (.attempt | test("^a-[0-9a-f]{32}$"))
      and (.number | type == "number" and . > 0) and (.messageId | test("^m-[0-9a-f]{32}$"))
      and .received == "received" and .ack == "processed" and .ackVersion == 3
      and (.launchTicketSha256 | test("^[0-9a-f]{64}$"))
      and .launchTicketSha256 == .closeTicketSha256 and .cleanup == "confirmed")
  | "BUNDLED_CODEX_RECEIPT " + ({mode,binarySha256,dispatch,round,attempt,number,messageId,received,ack,ackVersion,
     launchTicketSha256,closeTicketSha256,cleanup} | tojson)
'
emit_once 'group=user-codex-smoke fixture_exit=0 cleanup_exit=0 elf_check_exit=0'
verify_elf
printf 'WFC_CANDIDATE_HOSTED_PASS elf_sha256=%s\n' "$expected"
