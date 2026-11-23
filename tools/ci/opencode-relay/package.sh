#!/bin/bash
# Package the frozen release using the #20 layout and verify the extracted binary.
set -euo pipefail
ci_dir=$(cd "$(dirname "$0")" && pwd)
binary="target/$TARGET/release/wfc"
sha256sum --check --strict "$EVIDENCE/binary.sha256"
work=$(mktemp -d "$RUNNER_TEMP/wfc-opencode-package.XXXXXXXX")
stage="$work/stage"
unpacked="$work/unpacked"
mkdir -p "$stage" "$unpacked"
install -m 755 "$binary" "$stage/wfc"
python3 -Bu tools/collect_notices.py "$TARGET" "$stage/bundled-notices"
git rev-parse HEAD > "$stage/COMMIT.txt"
(cd "$stage" && sha256sum wfc > BINARY-SHA256SUMS)
archive="$EVIDENCE/wfc-linux-x64-$PRODUCT_SHA.tar.gz"
tar -czf "$archive" -C "$stage" .
python3 -Bu "$ci_dir/verify-notices.py" "$TARGET" "$archive"
tar -xzf "$archive" -C "$unpacked"
(cd "$unpacked" && sha256sum --check --strict BINARY-SHA256SUMS)
test "$(cat "$unpacked/COMMIT.txt")" = "$PRODUCT_SHA"
cmp -- "$binary" "$unpacked/wfc"
version=$("$unpacked/wfc" --version)
printf '%s\n' "$version"
[[ "$version" == *"(Rust, $TARGET, release, build $PRODUCT_SHA)" ]]
readelf -hW -lW -dW -VW "$unpacked/wfc"
dependencies=$(ldd "$unpacked/wfc")
printf '%s\n' "$dependencies"
[[ "$dependencies" != *'not found'* ]]
sha256sum "$archive"
