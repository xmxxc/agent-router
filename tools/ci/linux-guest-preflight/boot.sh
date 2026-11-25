#!/usr/bin/env bash
set -euo pipefail

work=$1
evidence=$2
checkpoint=${3:-}
image="$work/image"
drives=(-drive "if=virtio,index=0,format=raw,file=$image/rootfs.ext4,snapshot=on")

if [[ -n "$checkpoint" ]]; then
    payload="$work/payload"
    disk="$work/checkpoint.ext4"
    umask 022
    mkdir -p "$payload"
    cp -R -- "$checkpoint" "$payload/checkpoint"
    printf '%s\n' "$PRODUCT_SHA" "$TARGET" "$ARTIFACT_ID" "$ARTIFACT_DIGEST" \
        "$MANIFEST_SHA256" "$PACKAGE_SHA256" "$ELF_SHA256" > "$payload/identity"
    payload_kib=$(du -sk -- "$payload" | cut -f1)
    truncate -s "$((payload_kib + 32768))K" "$disk"
    mke2fs -q -F -t ext4 -d "$payload" "$disk"
    sha256sum "$disk" | tee "$evidence/checkpoint-disk.sha256"
    drives+=(-drive "if=virtio,index=1,format=raw,file=$disk,readonly=on")
fi

rc=0
timeout --kill-after=10s 180s qemu-system-x86_64 -accel tcg -smp 1 -m 768 \
    -display none -monitor none -serial stdio -no-reboot -nic none \
    -kernel "$image/vmlinuz" -initrd "$image/initrd.img" "${drives[@]}" \
    -append 'root=/dev/vda rw console=ttyS0 systemd.unified_cgroup_hierarchy=1 wfc_guest_preflight=1' \
    2>&1 | tee "$evidence/guest.log" || rc=$?
printf 'qemu_exit=%s\n' "$rc" | tee "$evidence/guest-exit.log"
test "$rc" -eq 0
grep -xF 'WFC_CLEAN_GUEST_PREFLIGHT_PASS' <(tr -d '\r' < "$evidence/guest.log")
if [[ -n "$checkpoint" ]]; then
    grep -xF 'WFC_DEFAULT_BUNDLED_PRODUCT_PASS' <(tr -d '\r' < "$evidence/guest.log")
fi
grep -F 'reboot: Power down' "$evidence/guest.log"
