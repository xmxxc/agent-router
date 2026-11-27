#!/usr/bin/env bash
set -euo pipefail

verified=${1:?usage: check-linux-release-inputs.sh VERIFIED_MANIFEST}
manifest=vendor/INPUTS.sha256
printf '%s  %s\n' 6e3d1e5cf38e0e93e6253078f26c61d12caedd388585b2699b9865bebb4115c1 \
    "$manifest" | sha256sum --check --strict

# P carries the older manifest. Replace only the 12 stale entries with hashes
# from P, then verify every entry against the checked-out product.
awk '
    NR == FNR { current[$2] = $1; next }
    $2 in current { print current[$2] "  " $2; delete current[$2]; next }
    { print }
    END { if (length(current)) exit 1 }
' - "$manifest" > "$verified" <<'SHA256'
370b4559be30fb11a6f519ed79d8d7d4278570d91d15653292313450e2aef655  src/bundled/mod.rs
dffcaaa551eab9dff2886cf5973f9d86b20595b4d456410f3bd2538b0a444a97  src/workflow.rs
8f4b8cb0fef9e32b53ac248179f88c6d8f229d3a11b77fec659e9dc5070ebaf2  src/workflow/backend.rs
7d270475225ec15422446a778046601bda10f8503457d4e5a03f19404bf79950  src/workflow/messages.rs
4517bfec20d3897fa7d4b4493d29e856d66d2983d1aad6be96485cbf2264dbe1  src/workflow/messages/recovery.rs
097b22841c9d65e63d0e95a5de1ba39eae403042c29f9051f4919a8fa1879cff  src/workflow/messages/recovery/progress.rs
4b53f767cdd72ebb7c32413db4bc8780c32780c1dca03ede31f223eaed9d098d  src/workflow/messages/relay.rs
5394f055184bbdb05e9f0e257a33781379972275fa587b8cf4e35c431afd0a7f  src/workflow/messages/relay/codex.rs
294f9543cfa8cd25ad0ba6aae5acfd028e2618ade596e344470803069e903078  src/workflow/messages/relay/queue_owner.rs
b0ce46f1647982b1a7794132db6c627c9108a6f3fa53d2c60edb85f3d5f37867  src/workflow/resume.rs
0b1541200cb20806e2a955abf09798b66648c343680c5651ef54f651255d0d66  src/workflow/view.rs
58b3c09388d561a49526a052ff96c1712ce97e843ae8a49bad0f5856d4569c51  tests/relay.py
SHA256

sha256sum --check --strict "$verified"
sha256sum "$verified"
