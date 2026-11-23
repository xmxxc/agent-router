import ast
import hashlib
import re
import subprocess
import sys
from pathlib import Path


METHOD = "test_public_resume_after_reclaim_preserves_history"
APPROVED_COMMIT = "7441bf7c38eeb6aba6d2c7ad71cd6c9640d8ac42"
PATCH_SHA256 = "3352bcc9d9784cb99adb975f0c04d7a5a21bd6d9570cf8f9f3e8f706771af6d8"
PRODUCT_SHA256 = "ced87ceed997a912c35190e939569e485ee07356d8916984982cdf407e36075b"
OVERLAY_SHA256 = "35ac6fce7f3fe7305f4cca85ac6c232deaee8c89f11b574d0dafb4fb4614266d"


def checked_bytes(path, expected):
    data = path.read_bytes()
    actual = hashlib.sha256(data).hexdigest()
    print(f"{actual}  {path}", flush=True)
    if actual != expected:
        raise SystemExit(f"SHA-256 mismatch for {path}")
    return data


def outside_method(source):
    backend, = [
        node for node in ast.parse(source).body
        if isinstance(node, ast.ClassDef) and node.name == "Backend"
    ]
    method, = [
        node for node in backend.body if isinstance(node, ast.FunctionDef) and node.name == METHOD
    ]
    lines = source.splitlines(keepends=True)
    return lines[:method.lineno - 1], lines[method.end_lineno:]


def shift_hunk(match):
    # The method moved by 56 lines at the fixed product pin. The final hunk
    # only inserts evidence, so git apply cannot locate it by matching lines.
    old, old_count, new, new_count = match.groups()
    return b"@@ -%d%s +%d%s @@" % (int(old) + 56, old_count, int(new) + 56, new_count)


if __name__ == "__main__":
    product_fixture, scratch = map(Path, sys.argv[1:])
    print(f"Approved patch source: {APPROVED_COMMIT}")
    approved_patch = checked_bytes(Path(__file__).with_name("callback-fixture.patch"), PATCH_SHA256)
    original = checked_bytes(product_fixture, PRODUCT_SHA256)
    overlay = scratch / "tools" / "check_tmux.py"
    overlay.parent.mkdir()
    overlay.write_bytes(original)
    rebased_patch = re.sub(rb"(?m)^@@ -(\d+)(,\d+|) \+(\d+)(,\d+|) @@", shift_hunk, approved_patch)
    subprocess.run(
        ["git", "apply", "--no-index", "--unidiff-zero", "-"],
        cwd=scratch, input=rebased_patch, check=True,
    )
    updated = checked_bytes(overlay, OVERLAY_SHA256)
    if outside_method(original) != outside_method(updated):
        raise SystemExit(f"Fixture changed outside Backend.{METHOD}")
    print(f"Only Backend.{METHOD} changed")
