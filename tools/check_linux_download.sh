#!/usr/bin/env bash
set -euo pipefail

cd -- "$1"
sha256_pattern='^[0-9a-f]{64}$'
[[ "$PRODUCT_SHA" =~ ^[0-9a-f]{40}$ && "$ARTIFACT_ID" =~ ^[1-9][0-9]*$ &&
    "$ARTIFACT_DIGEST" =~ $sha256_pattern && "$MANIFEST_SHA256" =~ $sha256_pattern &&
    "$PACKAGE_SHA256" =~ $sha256_pattern && "$ELF_SHA256" =~ $sha256_pattern ]]

archive="wfc-linux-x64-$PRODUCT_SHA.tar.gz"
printf '%s  %s\n' "$MANIFEST_SHA256" SHA256SUMS | sha256sum --check --strict
sha256sum --check --strict SHA256SUMS
printf '%s  %s\n' "$PACKAGE_SHA256" "$archive" | sha256sum --check --strict
read -r build_digest _ < binary.sha256
[[ "$build_digest" == "$ELF_SHA256" ]]
printf 'artifact_id=%s artifact_zip_sha256=%s manifest_sha256=%s package_sha256=%s elf_sha256=%s product=%s\n' \
    "$ARTIFACT_ID" "$ARTIFACT_DIGEST" "$MANIFEST_SHA256" "$PACKAGE_SHA256" "$ELF_SHA256" "$PRODUCT_SHA"
