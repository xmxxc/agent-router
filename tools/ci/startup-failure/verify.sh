#!/bin/bash
# Run the repaired fixture against the separate, frozen product checkout.
set -euo pipefail
test "${1:-}" = herdr-recovery
test -x "${HERDR_BIN:?verified Herdr binary required}"
ci=$(cd "$(dirname "$0")/../../.." && pwd)
binary=$(realpath "target/$TARGET/release/wfc")
version=$("$binary" --version)
[[ "$version" == *"(Rust, $TARGET, release, build $PRODUCT_SHA)" ]]
test "$(git rev-parse HEAD)" = "$PRODUCT_SHA"
test "$(git rev-parse 'HEAD^{tree}')" = "$PRODUCT_TREE"
printf '%s  %s
' "$FIXTURE_TEST_SHA" "$ci/tests/herdr_public.py" | sha256sum --check --strict

command=(python3 -Bu "$ci/tests/herdr_public.py" "$HERDR_BIN" "$binary" -v
    PublicLifecycle.test_runtime_read_evidence_and_independent_recovery)
status=0
{
    printf 'selector:'
    printf ' %q' "${command[@]}"
    printf '
'
    printf 'binary=%s
version=%s
fixture=%s
' "$binary" "$version" "$FIXTURE_SHA"
    sha256sum "$binary" "$ci/tests/herdr_public.py"
    python3 "$ci/tools/ci/startup-failure/reap.py" "${command[@]}"
} 2>&1 | tee "$EVIDENCE/herdr-recovery.log" || status=$?
printf 'herdr-recovery exit=%s
' "$status" | tee -a "$EVIDENCE/results.log"
exit "$status"
