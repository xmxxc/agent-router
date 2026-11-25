#!/usr/bin/env bash
set -euo pipefail

input=/run/wfc/checkpoint
tools=/usr/local/libexec
[[ "$(findmnt -n -o SOURCE --mountpoint "$input")" == /dev/vdb ]]
options=$(findmnt -n -o OPTIONS --mountpoint "$input")
for option in ro nodev nosuid noexec; do
    [[ ",$options," == *",$option,"* ]]
done
printf 'checkpoint_mount=%s source=/dev/vdb options=%s\n' "$input" "$options"

mapfile -t identity < "$input/identity"
[[ "${#identity[@]}" == 7 && "${identity[1]}" == x86_64-unknown-linux-gnu ]]
export PRODUCT_SHA="${identity[0]}" ARTIFACT_ID="${identity[2]}" ARTIFACT_DIGEST="${identity[3]}"
export MANIFEST_SHA256="${identity[4]}" PACKAGE_SHA256="${identity[5]}" ELF_SHA256="${identity[6]}"
/bin/bash "$tools/check_linux_download.sh" "$input/checkpoint"

IFS= read -r membership < "/proc/$$/cgroup"
[[ "$(id -u)" == 1000 && "$membership" == 0::/system.slice/wfc-guest-preflight.service ]]
export WFC_BUNDLED_CGROUP_ROOT="/sys/fs/cgroup${membership#0::}"
exec /bin/bash "$tools/check_linux_runtime.sh" "$input/checkpoint" \
    "$PRODUCT_SHA" "${identity[1]}" "$MANIFEST_SHA256" clean
