#!/usr/bin/env bash
set -euo pipefail

work=$1
evidence=$2
mode=${3:-preflight}
SNAPSHOT=https://snapshot.ubuntu.com/ubuntu/20261124T000000Z
GUEST_BASE=docker.io/library/ubuntu:22.04@sha256:b682e31336cc8e0a73ed91ab32b7a95e32591c5ff09994490a20accf9b3600f9
KERNEL_RELEASE=5.15.0-204-generic
QEMU_VERSION=1:8.2.2+ds-0ubuntu1.18

guest=$(cd -- "$(dirname -- "$0")" && pwd)
tools=$(cd -- "$guest/../.." && pwd)
config="$work/context/guest-config"
mkdir -p "$config/usr/local/libexec" "$evidence"
cp "$guest"/{Dockerfile,guest-preflight.sh,wfc-guest-preflight.service} "$work/context"
if [[ "$mode" == checkpoint ]]; then
    unit_dir="$config/etc/systemd/system"
    libexec="$config/usr/local/libexec"
    install -Dm644 "$guest/run-wfc-checkpoint.mount" "$unit_dir/run-wfc-checkpoint.mount"
    install -Dm644 "$guest/checkpoint.conf" "$unit_dir/wfc-guest-preflight.service.d/checkpoint.conf"
    cp "$guest/run-checkpoint.sh" "$tools"/check_linux_{download,runtime,bundled_package}.sh "$libexec"
fi
{
  git -C "$guest" show -s --format='workflow=%H tree=%T parents=%P'
  printf 'guest_base=%s\nsnapshot=%s\n' "$GUEST_BASE" "$SNAPSHOT"
  fetch() {
    local source=$1 digest=$2 destination=$3
    curl --fail --silent --show-error --location --retry 2 \
      --connect-timeout 10 --max-time 90 "$SNAPSHOT/$source" -o "$destination"
    printf '%s  %s\n' "$digest" "$destination" | sha256sum --check --strict
    printf 'source=%s sha256=%s\n' "$SNAPSHOT/$source" "$digest"
  }
  fetch pool/main/q/qemu/qemu-system-x86_8.2.2+ds-0ubuntu1.18_amd64.deb \
    14602e262627adac030329d2cecfee5f6c0938b34566ff2ea2d4c9a9eb02430d "$work/qemu-x86.deb"
  fetch pool/main/q/qemu/qemu-system-common_8.2.2+ds-0ubuntu1.18_amd64.deb \
    0a0b744f31e87d72dd0465436b8dfbb1ef9f574ecd0d5a2e9dd8c5cb7d0fef1f "$work/qemu-common.deb"
  fetch pool/main/q/qemu/qemu-system-data_8.2.2+ds-0ubuntu1.18_all.deb \
    a14b88d864859bd61c8a3274971da4ecb7da6cec15be6c265d0d411f783d5f2e "$work/qemu-data.deb"
  fetch pool/main/l/linux-signed/linux-image-5.15.0-204-generic_5.15.0-204.214_amd64.deb \
    f64db5a37befdde1ca0bef10a5826614482ee903c3163e57ed776d97a5919c75 "$work/context/linux-image.deb"
  fetch pool/main/l/linux/linux-modules-5.15.0-204-generic_5.15.0-204.214_amd64.deb \
    e205a3933a1f6551dc04e3975b46df119fe12a0ba4b5ea37bdcd30862ad69a98 "$work/context/linux-modules.deb"
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends "$work/qemu-x86.deb" "$work/qemu-common.deb" "$work/qemu-data.deb"
  for package in qemu-system-x86 qemu-system-common qemu-system-data; do
    test "$(dpkg-query -W -f='${Version}' "$package")" = "$QEMU_VERSION"
  done
  qemu-system-x86_64 --version
  dpkg-query -W qemu-system-x86 qemu-system-common qemu-system-data seabios
  sha256sum "$work/context/"{Dockerfile,guest-preflight.sh,wfc-guest-preflight.service}
  if [[ "$mode" == checkpoint ]]; then
    sha256sum "$libexec/"*.sh "$unit_dir/run-wfc-checkpoint.mount" "$unit_dir/wfc-guest-preflight.service.d/checkpoint.conf"
  fi
} 2>&1 | tee "$evidence/inputs.log"

docker build --platform linux/amd64 --progress plain --output "type=local,dest=$work/image" \
  --build-arg BASE_IMAGE="$GUEST_BASE" --build-arg KERNEL_RELEASE="$KERNEL_RELEASE" \
  "$work/context" 2>&1 | tee "$evidence/image.log"
(cd "$work/image" && sha256sum vmlinuz initrd.img rootfs.ext4) | tee "$evidence/guest-image.sha256"
