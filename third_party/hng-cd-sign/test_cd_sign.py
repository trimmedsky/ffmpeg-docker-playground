"""cd_sign.py against ../fixtures/cd-sign.json, which hng's `cd_sign` test also checks (the
payload and header bytes, and noded's verifier on the fixture's JWS)."""
import base64
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature, encode_dss_signature

import cd_sign

HERE = Path(__file__).resolve().parent
FIXTURE = json.loads((HERE.parent / "fixtures" / "cd-sign.json").read_text(encoding="utf-8"))


def unb64(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def verify(public_key, jws):
    """The checks of noded-core `Jws::parse` + `verify`: three parts, 64-byte low-S signature."""
    head, payload, sig = jws.split(".")
    raw = unb64(sig)
    if len(raw) != 64:
        raise InvalidSignature("not 64 bytes")
    r, s = int.from_bytes(raw[:32], "big"), int.from_bytes(raw[32:], "big")
    if s > cd_sign.N // 2:
        raise InvalidSignature("high-S")
    public_key.verify(encode_dss_signature(r, s), f"{head}.{payload}".encode("ascii"), ec.ECDSA(hashes.SHA256()))
    return unb64(head), unb64(payload)


def run(argv, env=None):
    out, err = io.StringIO(), io.StringIO()
    with patch.dict(os.environ, env or {}, clear=env is not None), \
            contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = cd_sign.main(argv)
        except SystemExit as e:  # argparse's usage errors
            code = e.code
    return code, out.getvalue(), err.getvalue()


def cli_args(args):
    return [f"--{k}={v}" for k, v in args.items()]


class FixtureTest(unittest.TestCase):
    def setUp(self):
        self.key = cd_sign.load_key(FIXTURE["test_cd_key"])
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)

    def test_kid_is_the_spki_sha256(self):
        self.assertEqual(cd_sign.key_id(self.key.public_key()), FIXTURE["kid"])

    def test_every_accepted_case_signs_the_fixture_bytes(self):
        self.assertGreaterEqual(len(FIXTURE["cases"]), 6)
        for case in FIXTURE["cases"]:
            with self.subTest(case["name"]):
                path = self.dir / "artifact"
                path.write_bytes(base64.b64decode(case["file_base64"]))
                [summary] = cd_sign.sign_files(self.key, [path], **case["args"])
                payload = json.loads(case["payload"])
                self.assertEqual(summary, {"file": str(path), "jws": str(path) + ".cd.jws",
                                           "sha256": payload["artifact"]["sha256"],
                                           "size": payload["artifact"]["size"]})
                head, body = verify(self.key.public_key(), Path(summary["jws"]).read_text(encoding="ascii"))
                self.assertEqual(head, case["header"].encode("utf-8"))
                self.assertEqual(body, case["payload"].encode("utf-8"))

    def test_the_fixture_jws_carries_the_fixture_bytes(self):
        for case in FIXTURE["cases"]:
            with self.subTest(case["name"]):
                head, body = verify(self.key.public_key(), case["jws"])
                self.assertEqual((head, body), (case["header"].encode("utf-8"), case["payload"].encode("utf-8")))

    def test_every_rejected_case_is_refused_and_writes_nothing(self):
        path = self.dir / "artifact"
        path.write_bytes(b"x")
        self.assertGreaterEqual(len(FIXTURE["rejected"]), 30)
        for case in FIXTURE["rejected"]:
            with self.subTest(case["name"]):
                with self.assertRaises(cd_sign.CdSignError):
                    cd_sign.sign_files(self.key, [path], **case["args"])
                args = cli_args(case["args"])
                code, out, _ = run([*args, str(path)], {"CD_SIGNING_KEY": FIXTURE["test_cd_key"]})
                self.assertNotEqual(code, 0)
                self.assertEqual(out, "")
                self.assertFalse(cd_sign.jws_path(path).exists())


class CliTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.files = [self.dir / "a-x86_64-linux", self.dir / "b-x86_64-linux"]
        for i, f in enumerate(self.files):
            f.write_bytes(b"bytes %d" % i)
        self.args = cli_args(FIXTURE["cases"][0]["args"])
        self.public = cd_sign.load_key(FIXTURE["test_cd_key"]).public_key()

    def test_key_from_the_default_env_signs_every_file(self):
        code, out, err = run([*self.args, *map(str, self.files)], {"CD_SIGNING_KEY": FIXTURE["test_cd_key"]})
        self.assertEqual((code, err), (0, ""))
        lines = [json.loads(line) for line in out.splitlines()]
        self.assertEqual([line["file"] for line in lines], [str(f) for f in self.files])
        for line, f in zip(lines, self.files):
            _, body = verify(self.public, Path(line["jws"]).read_text(encoding="ascii"))
            self.assertEqual(json.loads(body)["artifact"], {"sha256": line["sha256"], "size": f.stat().st_size})
        self.assertNotIn("PRIVATE", out + err)

    def test_key_from_another_env_or_a_sec1_file(self):
        code, _, _ = run(["--key-env", "MY_KEY", *self.args, str(self.files[0])], {"MY_KEY": FIXTURE["test_cd_key"]})
        self.assertEqual(code, 0)
        sec1 = cd_sign.load_key(FIXTURE["test_cd_key"]).private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption())
        key_file = self.dir / "cd.pem"
        key_file.write_bytes(sec1)
        code, _, _ = run(["--key-file", str(key_file), *self.args, str(self.files[0])], {})
        self.assertEqual(code, 0)
        head, _ = verify(self.public, cd_sign.jws_path(self.files[0]).read_text(encoding="ascii"))
        self.assertEqual(json.loads(head)["kid"], FIXTURE["kid"])

    def test_bad_keys_are_refused_without_echoing_them(self):
        p384 = ec.generate_private_key(ec.SECP384R1()).private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()).decode()
        encrypted = cd_sign.load_key(FIXTURE["test_cd_key"]).private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
            serialization.BestAvailableEncryption(b"pass")).decode()
        garbage = "-----BEGIN PRIVATE KEY-----\nc2VjcmV0LW1hcmtlcg==\n-----END PRIVATE KEY-----\n"
        for name, env in [("unset", {}), ("empty", {"CD_SIGNING_KEY": ""}), ("P-384", {"CD_SIGNING_KEY": p384}),
                          ("encrypted", {"CD_SIGNING_KEY": encrypted}), ("garbage", {"CD_SIGNING_KEY": garbage})]:
            with self.subTest(name):
                code, out, err = run([*self.args, str(self.files[0])], env)
                self.assertEqual((code, out), (1, ""))
                self.assertIn("CD", err)
                for secret in [v for v in env.values() if v] + ["c2VjcmV0", "BEGIN"]:
                    self.assertNotIn(secret, err)
                self.assertFalse(cd_sign.jws_path(self.files[0]).exists())
        code, _, err = run(["--key-file", str(self.dir / "missing.pem"), *self.args, str(self.files[0])], {})
        self.assertEqual(code, 1)

    def test_a_missing_file_stops_before_anything_is_written(self):
        code, out, err = run([*self.args, str(self.files[0]), str(self.dir / "missing")],
                             {"CD_SIGNING_KEY": FIXTURE["test_cd_key"]})
        self.assertEqual((code, out), (1, ""))
        self.assertIn("missing", err)
        self.assertFalse(cd_sign.jws_path(self.files[0]).exists())

    def test_a_failed_write_leaves_no_signature_and_no_summary(self):
        (self.dir / (self.files[1].name + ".cd.jws.tmp")).mkdir()  # the second file's staging path
        code, out, err = run([*self.args, *map(str, self.files)], {"CD_SIGNING_KEY": FIXTURE["test_cd_key"]})
        self.assertEqual((code, out), (1, ""))
        self.assertIn("cannot write", err)
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()),
                         sorted([f.name for f in self.files] + [self.files[1].name + ".cd.jws.tmp"]))

    def test_a_high_s_signature_is_sent_in_low_s_form(self):
        key = cd_sign.load_key(FIXTURE["test_cd_key"])
        payload = b"{}"
        for _ in range(64):  # ECDSA is randomized: find one signature that came out high-S
            real = key.sign
            seen = []

            def sign(data, algorithm, real=real, seen=seen):
                der = real(data, algorithm)
                seen.append(decode_dss_signature(der)[1])
                return der
            with patch.object(type(key), "sign", lambda self, d, a: sign(d, a)):
                jws = cd_sign.sign(key, payload)
            if seen[0] > cd_sign.N // 2:
                break
        else:
            self.fail("no high-S signature in 64 tries")
        _, body = verify(self.public, jws)  # verify() refuses high-S
        self.assertEqual(body, payload)

    def test_a_key_env_that_is_not_utf8_is_refused(self):
        with patch.dict(os.environ, {"CD_SIGNING_KEY": "\udcff"}):
            with self.assertRaises(cd_sign.CdSignError):
                cd_sign.read_key(None, "CD_SIGNING_KEY")

    def test_isolated_interpreter_entry_point(self):
        env = {k: v for k, v in os.environ.items() if k != "CD_SIGNING_KEY"}
        env["CD_SIGNING_KEY"] = FIXTURE["test_cd_key"]
        done = subprocess.run([sys.executable, "-I", str(HERE / "cd_sign.py"), *self.args, str(self.files[0])],
                              env=env, capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout)["file"], str(self.files[0]))
        verify(self.public, cd_sign.jws_path(self.files[0]).read_text(encoding="ascii"))


if __name__ == "__main__":
    unittest.main()
