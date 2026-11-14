#!/usr/bin/env bash
# agent/rust-toolchain.toml is the Rust version. CI installs it from that file; the Dockerfile's
# `FROM rust:<version>-...` stage cannot read it, and Cargo.toml's `rust-version` is the same minor
# version, so this checks that both agree with it.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/agent/rust-toolchain.toml")
image=$(sed -n 's/^FROM rust:\([0-9][0-9.]*\)-.*/\1/p' "$root/Dockerfile")
msrv=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$root/agent/Cargo.toml")
[[ "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "agent/rust-toolchain.toml: channel ${toolchain:-?} is not an exact version" >&2; exit 1; }
[ "$image" = "$toolchain" ] || { echo "Dockerfile: FROM rust:${image:-?}, agent/rust-toolchain.toml: $toolchain" >&2; exit 1; }
[ "$msrv" = "${toolchain%.*}" ] || { echo "agent/Cargo.toml: rust-version ${msrv:-?}, agent/rust-toolchain.toml: $toolchain" >&2; exit 1; }
echo "ok rust $toolchain"
