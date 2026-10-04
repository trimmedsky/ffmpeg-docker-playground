#!/usr/bin/env python3
"""A throwaway ES256 JWT issuer for tests and local trials.

It uses only the Python standard library and the `openssl` command, so the tests need
no Python crypto package. Do not use it as a production issuer.

    python3 tests/jwt_issuer.py init DIR [--kid KID]
        Writes DIR/signing-key.pem (private, keep it) and DIR/jwks.json (public; point
        FFMPEG_AGENT_AUTH_JWKS_FILE at it).
    python3 tests/jwt_issuer.py token DIR --aud AUDIENCE [--sub SUB] [--iss ISS]
                                          [--typ TYP] [--lifetime SECS] [--kid KID]
        Prints a fresh JWT signed with DIR/signing-key.pem.
"""
import argparse
import base64
import json
from pathlib import Path
import subprocess
import time

KEY_FILE = "signing-key.pem"
JWKS_FILE = "jwks.json"


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def _der_length(data, offset):
    first = data[offset]
    if first < 0x80:
        return first, offset + 1
    count = first & 0x7F
    return int.from_bytes(data[offset + 1:offset + 1 + count], "big"), offset + 1 + count


def der_to_raw(der):
    """ASN.1 ECDSA-Sig-Value (what openssl prints) -> the fixed 64-byte r || s of JWS."""
    assert der[0] == 0x30, "not a DER sequence"
    _, offset = _der_length(der, 1)
    halves = []
    for _ in range(2):
        assert der[offset] == 0x02, "not a DER integer"
        length, offset = _der_length(der, offset + 1)
        halves.append(int.from_bytes(der[offset:offset + length], "big").to_bytes(32, "big"))
        offset += length
    return halves[0] + halves[1]


class Issuer:
    """One P-256 key and the default header and claims of its tokens.
    Calling the object returns a fresh default token."""

    def __init__(self, directory, kid="test-key", audience="ffmpeg-agent", subject="example-caller",
                 issuer=None, typ=None, create=True):
        self.directory = Path(directory)
        self.key = self.directory / KEY_FILE
        self.kid, self.audience, self.subject, self.issuer, self.typ = kid, audience, subject, issuer, typ
        self.issued = []
        if create:
            self.directory.mkdir(parents=True, exist_ok=True)
            subprocess.run(["openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(self.key)],
                           check=True, capture_output=True)
            self.key.chmod(0o600)

    def jwk(self, kid=None):
        der = subprocess.run(["openssl", "ec", "-in", str(self.key), "-pubout", "-outform", "DER"],
                             check=True, capture_output=True).stdout
        point = der[-65:]
        assert point[0] == 4, "uncompressed P-256 point expected"
        return {"kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": kid or self.kid,
                "x": b64url(point[1:33]), "y": b64url(point[33:])}

    def write_jwks(self, path=None, *keys):
        """Atomically replace the JWKS file (as a key rotation would) with `keys`, or
        with this issuer's key alone."""
        path = Path(path or self.directory / JWKS_FILE)
        temporary = path.with_name(path.name + ".tmp")
        temporary.write_text(json.dumps({"keys": list(keys) if keys else [self.jwk()]}))
        temporary.replace(path)
        return path

    def sign(self, header, claims):
        """Signs exactly the given header and claims (dicts, or raw JSON text)."""
        encode = lambda v: b64url((v if isinstance(v, str) else json.dumps(v, separators=(",", ":"))).encode())
        data = encode(header) + "." + encode(claims)
        der = subprocess.run(["openssl", "dgst", "-sha256", "-sign", str(self.key)], input=data.encode(),
                             check=True, capture_output=True).stdout
        token = data + "." + b64url(der_to_raw(der))
        self.issued.append(token)
        return token

    def token(self, lifetime=60, header=None, **claims):
        """A token with the defaults; `header` and `claims` override them, None removes."""
        now = int(time.time())
        body = {"iss": self.issuer, "sub": self.subject, "aud": self.audience, "iat": now, "exp": now + lifetime}
        body.update(claims)
        jose = {"alg": "ES256", "typ": self.typ, "kid": self.kid, **(header or {})}
        drop = lambda d: {k: v for k, v in d.items() if v is not None}
        return self.sign(drop(jose), drop(body))

    def __call__(self):
        return self.token()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    init = sub.add_parser("init")
    init.add_argument("directory")
    init.add_argument("--kid", default="test-key")
    token = sub.add_parser("token")
    token.add_argument("directory")
    token.add_argument("--aud", required=True)
    token.add_argument("--sub", default="example-caller")
    token.add_argument("--iss")
    token.add_argument("--typ")
    token.add_argument("--lifetime", type=int, default=60)
    token.add_argument("--kid", default="test-key")
    args = parser.parse_args()
    if args.command == "init":
        issuer = Issuer(args.directory, kid=args.kid)
        print(issuer.write_jwks())
    else:
        issuer = Issuer(args.directory, kid=args.kid, audience=args.aud, subject=args.sub, issuer=args.iss,
                        typ=args.typ, create=False)
        print(issuer.token(lifetime=args.lifetime))


if __name__ == "__main__":
    main()
