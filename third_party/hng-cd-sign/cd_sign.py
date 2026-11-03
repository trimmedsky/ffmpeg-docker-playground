"""CD signer of noded releases: for each artifact file, the metadata (repository, kind, name,
version, commit, arch, sha256 and size), signed with the repository's CD key (ES256 JWS), written
next to the file as `<file>.cd.jws` (docs/noded-release.md in saiya/home-net-gateway).

    CD_SIGNING_KEY="$(cat cd.pem)" python3 -I cd_sign.py --repository owner/name --kind binary \\
      --name app --version 1.2.3 --commit <40 hex> --arch x86_64-linux dist/app-x86_64-linux ...

The key (P-256, PKCS#8 or SEC1 PEM) is read from the environment variable `CD_SIGNING_KEY`
(`--key-env` names another one) or from `--key-file`. It is never printed. stdout gets one JSON
line per file: `{"file", "jws", "sha256", "size"}`.

The JSON and the JWS are byte for byte what noded's verifier expects: noded-core `release.rs`
(`CdMetadata`, `sign_metadata`) and `jws.rs` (`sign`), with the input rules of hng's `cd.rs`.
`../fixtures/cd-sign.json` pins both sides (test_cd_sign.py here, hng's `cd_sign` test there).
Python 3.11+, `cryptography` only (pinned with every wheel's hash in requirements.txt).
"""
import sys

if sys.version_info < (3, 11):
    sys.exit("cd_sign.py needs Python 3.11+")

import argparse
import base64
import hashlib
import json
import os
import re
from pathlib import Path

from cryptography.exceptions import UnsupportedAlgorithm
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature

TYP = "noded-cd-metadata+json"
SUFFIX = ".cd.jws"
KINDS = ("binary", "image")
DEFAULT_KEY_ENV = "CD_SIGNING_KEY"
MAX_VERSION_BYTES = 128
# The order of the P-256 group: a signature is sent with s <= N/2 (noded rejects the other form).
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551

_REPOSITORY_PART = re.compile(r"[A-Za-z0-9._-]{1,100}")
_COMMIT = re.compile(r"[0-9a-f]{40}")
_LABEL = re.compile(r"[a-z0-9][a-z0-9_.-]{0,63}")


class CdSignError(Exception):
    """A refused input, key or file. The message never carries key material."""


def b64(data):
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def check_describe(repository, kind, name, version, commit, arch):
    """The rules of hng's `cd::Describe` (and noded-core's `Label` for kind, name and arch)."""
    owner, slash, repo = repository.partition("/")
    if not slash or not all(_REPOSITORY_PART.fullmatch(p) and p not in (".", "..") for p in (owner, repo)):
        raise CdSignError(f"repository must be owner/name: {repository!r}")
    if not _COMMIT.fullmatch(commit):
        raise CdSignError(f"commit must be the full 40-hex hash: {commit!r}")
    if kind not in KINDS:
        raise CdSignError(f"kind must be binary or image: {kind!r}")
    for what, value in (("name", name), ("arch", arch)):
        if not _LABEL.fullmatch(value):
            raise CdSignError(f"{what} must be a label ([a-z0-9][a-z0-9_.-]*, at most 64 bytes): {value!r}")
    try:
        size = len(version.encode("utf-8"))
    except UnicodeEncodeError:
        raise CdSignError("version must be UTF-8") from None
    if not 1 <= size <= MAX_VERSION_BYTES:
        raise CdSignError(f"version must be 1..{MAX_VERSION_BYTES} bytes")


def load_key(pem):
    """A P-256 private key from PEM (bytes or str)."""
    try:
        if isinstance(pem, str):
            pem = pem.encode("utf-8")
        key = serialization.load_pem_private_key(pem.strip(), password=None)
    except (ValueError, TypeError, UnsupportedAlgorithm):  # UnicodeEncodeError is a ValueError
        raise CdSignError("the CD key is not an unencrypted private key in PEM (PKCS#8 or SEC1)") from None
    if not isinstance(key, ec.EllipticCurvePrivateKey) or not isinstance(key.curve, ec.SECP256R1):
        raise CdSignError("the CD key must be a P-256 key")
    return key


def key_id(public_key):
    """`cd:<sha256 of the SubjectPublicKeyInfo (DER)>`: the JWS `kid` (noded-core `SignerId::Cd`)."""
    spki = public_key.public_bytes(serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
    return "cd:" + hashlib.sha256(spki).hexdigest()


def hash_file(path):
    digest, size = hashlib.sha256(), 0
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def metadata(repository, kind, name, version, commit, arch, sha256, size):
    """The metadata in `CdMetadata`'s field order."""
    check_describe(repository, kind, name, version, commit, arch)
    return {"repository": repository, "kind": kind, "name": name, "version": version, "commit": commit,
            "arch": arch, "artifact": {"sha256": sha256, "size": size}}


def to_json(value):
    """serde_json's compact form: no spaces, non-ASCII as UTF-8, the same escapes."""
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def sign(key, payload):
    """Compact JWS of `payload` with the header `{alg, typ, kid}` and a low-S ES256 signature."""
    header = to_json({"alg": "ES256", "typ": TYP, "kid": key_id(key.public_key())})
    signing_input = b64(header) + "." + b64(payload)
    r, s = decode_dss_signature(key.sign(signing_input.encode("ascii"), ec.ECDSA(hashes.SHA256())))
    if s > N // 2:
        s = N - s
    return signing_input + "." + b64(r.to_bytes(32, "big") + s.to_bytes(32, "big"))


def jws_path(path):
    return Path(os.fspath(path) + SUFFIX)


def sign_files(key, files, repository, kind, name, version, commit, arch):
    """Sign every file. All or nothing: inputs are checked and every file is hashed and signed
    before anything is written, and each `.cd.jws` is staged as `.cd.jws.tmp` and renamed only once
    every one is staged. Returns the summaries."""
    check_describe(repository, kind, name, version, commit, arch)
    signed = []
    for path in files:
        if not Path(path).is_file():
            raise CdSignError(f"not a file: {path}")
        try:
            sha256, size = hash_file(path)
        except OSError as e:
            raise CdSignError(f"cannot read {path}: {e.strerror}") from None
        payload = to_json(metadata(repository, kind, name, version, commit, arch, sha256, size))
        signed.append(({"file": os.fspath(path), "jws": os.fspath(jws_path(path)), "sha256": sha256, "size": size},
                       sign(key, payload)))
    staged = []
    try:
        for summary, jws in signed:
            tmp = Path(summary["jws"] + ".tmp")
            tmp.write_text(jws, encoding="ascii")
            staged.append((tmp, summary["jws"]))
        for tmp, target in staged:
            os.replace(tmp, target)
    except OSError as e:
        for tmp, _ in staged:
            tmp.unlink(missing_ok=True)
        raise CdSignError(f"cannot write {e.filename}: {e.strerror}") from None
    return [summary for summary, _ in signed]


def read_key(key_file, key_env):
    if key_file is not None:
        try:
            return load_key(Path(key_file).read_bytes())
        except OSError as e:
            raise CdSignError(f"cannot read the CD key file: {e.strerror}") from None
    pem = os.environ.get(key_env)
    if not pem:
        raise CdSignError(f"{key_env} (the CD signing key, PEM) is not set")
    return load_key(pem)


def main(argv=None):
    p = argparse.ArgumentParser(description="Sign the CD metadata of artifacts (<file>.cd.jws next to each).")
    source = p.add_mutually_exclusive_group()
    source.add_argument("--key-file", help="PEM file of the CD key")
    source.add_argument("--key-env", default=DEFAULT_KEY_ENV,
                        help=f"environment variable holding the CD key's PEM (default {DEFAULT_KEY_ENV})")
    p.add_argument("--repository", required=True, help="owner/name the CD key is registered for")
    p.add_argument("--kind", required=True, choices=KINDS)
    p.add_argument("--name", required=True, help="release name")
    p.add_argument("--version", required=True)
    p.add_argument("--commit", required=True, help="full 40-hex git commit the files were built from")
    p.add_argument("--arch", required=True, help="the members document's platform.arch (x86_64-linux, ...)")
    p.add_argument("files", nargs="+")
    a = p.parse_args(argv)
    try:
        check_describe(a.repository, a.kind, a.name, a.version, a.commit, a.arch)
        key = read_key(a.key_file, a.key_env)
        for line in sign_files(key, a.files, a.repository, a.kind, a.name, a.version, a.commit, a.arch):
            print(json.dumps(line))
    except CdSignError as e:
        print(f"cd_sign.py: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
