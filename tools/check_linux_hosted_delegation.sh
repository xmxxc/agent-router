#!/usr/bin/env bash
set -euo pipefail

expected_uid=$1
unit=$2
shift 2
[[ "$(id -u)" == "$expected_uid" && "$expected_uid" -gt 0 ]]
IFS= read -r membership < "/proc/$$/cgroup"
[[ "$membership" == "0::/system.slice/$unit.service" ]]
export WFC_BUNDLED_CGROUP_ROOT="/sys/fs/cgroup${membership#0::}"

exec /bin/bash "$(dirname -- "$0")/check_linux_runtime.sh" "$@" hosted-delegated
