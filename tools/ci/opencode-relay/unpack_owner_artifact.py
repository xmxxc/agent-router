"""Verify the original #38 Ubuntu 22.04 artifact and stage only its pinned ELF."""
from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import re
import sys
import tarfile
from zipfile import ZipFile

RUN = 45233658606
WORKFLOW = "f4585dbecbe42f7b00836b117b9d916685772a91"
ARTIFACT = 14847864563
ZIP_HASH = "a607850f237a5eecbf73abd8a14490cf8e05ce4226ef33f63bdcbf1c523cc325"
PRODUCT = "453fe04cd18dfccd525c8070ebfe33e659146a1a"
TREE = "a30c73e0c4fdb8bab531079edcbf622e9f59d560"
PACKAGE_HASH = "e39e00d261ae39857fc7c2622344b6bd6ba832a7f33c56808c5b2aa36359b75b"
BINARY_HASH = "225bfd09b755d7e55e1044e72b4210f1888133af6635d7a1a5d201ac3fb8b8e8"


def digest(data):
    return sha256(data).hexdigest()


def verify(archive, metadata, output):
    raw = archive.read_bytes()
    record = json.loads(metadata.read_bytes())
    assert record["id"] == ARTIFACT and not record["expired"]
    assert record["name"] == f"linux-opencode-jammy-{RUN}-1"
    assert record["workflow_run"]["id"] == RUN
    assert record["workflow_run"]["head_sha"] == WORKFLOW
    assert len(raw) == record["size_in_bytes"] == 3565814
    assert record["digest"] == f"sha256:{ZIP_HASH}" and digest(raw) == ZIP_HASH
    print(f"Official artifact {ARTIFACT} from run {RUN}, ZIP SHA-256 {ZIP_HASH}")
    with ZipFile(BytesIO(raw)) as z:
        entries = z.namelist()
        assert len(entries) == len(set(entries))
        manifest = [line.split("  ", 1) for line in z.read("SHA256SUMS").decode().splitlines()]
        assert len(manifest) == 23
        assert {name for _, name in manifest} == set(entries) - {"SHA256SUMS"}
        for expected, name in manifest:
            assert re.fullmatch(r"[\w.+-]+", name) and name not in (".", "..")
            assert digest(z.read(name)) == expected, name
        print("23 artifact payloads match the original SHA256SUMS")
        run = z.read("run.log").decode().splitlines()
        assert {f"workflow={WORKFLOW}", f"run={RUN}", "attempt=1",
                f"expectedProduct={PRODUCT}", f"expectedTree={TREE}"} <= set(run)
        outcomes = z.read("outcomes.log").decode().splitlines()
        assert {"BUILD=success", "PACKAGE=success", "SUITES=failure",
                "firstFailedStep=SUITES"} <= set(outcomes)
        print("Original SUITES failure is preserved in the verified artifact")
        assert z.read("binary.sha256").decode() == (
            f"{BINARY_HASH}  target/x86_64-unknown-linux-gnu/release/wfc\n"
        )
        package = z.read(f"wfc-linux-x64-{PRODUCT}.tar.gz")
        assert digest(package) == PACKAGE_HASH
        print(f"Packaged tar SHA-256 {PACKAGE_HASH}")
    with tarfile.open(fileobj=BytesIO(package), mode="r:gz") as tar:
        def member(name):
            item, = [entry for entry in tar.getmembers() if entry.name == name]
            assert item.isfile(), name
            with tar.extractfile(item) as source:
                return source.read()

        assert member("./COMMIT.txt").decode() == PRODUCT + "\n"
        assert member("./BINARY-SHA256SUMS").decode() == f"{BINARY_HASH}  wfc\n"
        binary = member("./wfc")
        assert digest(binary) == BINARY_HASH
        assert binary.startswith(b"\x7fELF") and PRODUCT.encode() in binary
        with output.open("xb") as f:
            f.write(binary)
        output.chmod(0o755)
        print(f"ELF SHA-256 {BINARY_HASH} and embedded product SHA {PRODUCT}")


if __name__ == "__main__":
    verify(*(Path(value) for value in sys.argv[1:]))
