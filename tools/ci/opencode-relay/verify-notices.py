"""Verify curl-sys notice bytes and index entries in the distribution archive."""
from hashlib import sha256
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tomllib


def verify(target, release_archive):
    lock = Path("Cargo.lock").read_bytes()
    pin, = [p for p in tomllib.loads(lock.decode())["package"] if p["name"] == "curl-sys"]
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--format-version", "1", "--locked", "--offline",
        "--filter-platform", target,
    ]))
    package, = [p for p in metadata["packages"]
                if (p["name"], p["version"], p["source"]) == (pin["name"], pin["version"], pin["source"])]
    node, = [n for n in metadata["resolve"]["nodes"] if n["id"] == package["id"]]
    assert "static-curl" in node["features"], node["features"]

    name = f"{pin['name']}-{pin['version']}"
    source = Path(package["manifest_path"]).parent
    crate = source.parents[2] / "cache" / source.parent.name / f"{name}.crate"
    digest = sha256(crate.read_bytes()).hexdigest()
    assert digest == pin["checksum"], crate
    print(f"{crate.name} SHA-256 {digest}")

    with tarfile.open(crate, "r:gz") as upstream, tarfile.open(release_archive, "r:gz") as release:
        base = "./bundled-notices"
        assert release.extractfile(f"{base}/Cargo.lock").read() == lock
        index = json.load(release.extractfile(f"{base}/sources.json"))
        assert index["target"] == target
        assert index["cargo_lock_sha256"] == sha256(lock).hexdigest()
        record, = [r for r in index["cargo"] if (r["name"], r["version"], r["source"])
                   == (pin["name"], pin["version"], pin["source"])]
        for path in ("LICENSE", "curl/COPYING"):
            assert path in record["notices"], (name, path)
            expected = upstream.extractfile(f"{name}/{path}").read()
            assert expected == (source / path).read_bytes()
            assert expected == release.extractfile(f"{base}/cargo/{name}/{path}").read()
            print(f"{name}/{path} indexed and byte-identical, SHA-256 {sha256(expected).hexdigest()}")


if __name__ == "__main__":
    verify(*sys.argv[1:])
