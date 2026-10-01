#!/usr/bin/env python3
"""Vendor `crates/hng-auth` from home-net-gateway, or check the vendored copy.

    scripts/vendor-hng-auth.py update <path to home-net-gateway checkout> <revision>
    scripts/vendor-hng-auth.py check

`update` reads the files with `git show <revision>:<path>` (it never switches the
checkout's branch), writes them under vendor/hng-auth/, and records the revision
and each file's sha256 in vendor/hng-source.json.

`check` recomputes the sha256 of every vendored file and fails if anything was
edited, added or removed by hand. CI runs it; it needs no network and no HNG checkout.
Only the standard library is used.
"""

import hashlib
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEST = ROOT / "vendor" / "hng-auth"
SOURCE = ROOT / "vendor" / "hng-source.json"
UPSTREAM_DIR = "crates/hng-auth"
# Tests stay upstream: they are HNG's contract tests, not ours.
FILES = ["Cargo.toml", "src/lib.rs", "src/acl.rs", "src/client.rs", "src/jwt.rs", "src/model.rs", "src/request.rs"]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def update(repo: str, revision: str) -> None:
    full = subprocess.run(["git", "-C", repo, "rev-parse", revision + "^{commit}"],
                          check=True, capture_output=True, text=True).stdout.strip()
    hashes = {}
    for name in FILES:
        data = subprocess.run(["git", "-C", repo, "show", f"{full}:{UPSTREAM_DIR}/{name}"],
                              check=True, capture_output=True).stdout
        out = DEST / name
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(data)
        hashes[name] = sha256(data)
    SOURCE.write_text(json.dumps({
        "repository": "saiya/home-net-gateway",
        "revision": full,
        "path": UPSTREAM_DIR,
        "files": hashes,
    }, indent=2) + "\n")
    print(f"vendored {UPSTREAM_DIR} at {full}")


def check() -> None:
    recorded = json.loads(SOURCE.read_text())["files"]
    present = sorted(str(p.relative_to(DEST)) for p in DEST.rglob("*") if p.is_file())
    bad = 0
    if present != sorted(recorded):
        print(f"FAIL  file set differs: vendored={present} recorded={sorted(recorded)}")
        bad += 1
    for name, digest in recorded.items():
        path = DEST / name
        actual = sha256(path.read_bytes()) if path.exists() else "(missing)"
        if actual != digest:
            print(f"FAIL  {name}: {actual} != recorded {digest}")
            bad += 1
        else:
            print(f"ok    {name}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "update":
        update(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 2 and sys.argv[1] == "check":
        check()
    else:
        sys.exit(__doc__)
