#!/usr/bin/env bash
# Checks that each vendored directory given is the byte copy its SOURCE.json records: the tree
# hash of saiya/devkit scripts/vendor-check.py (SHA-256 of the `<sha256>  ./<path>` lines of every
# file but SOURCE.json, sorted bytewise). Re-vendor with that script's `update`, not by hand.
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: $0 VENDORED_DIR..." >&2; exit 2; }
for dir in "$@"; do
  want=$(jq -r .sha256 "$dir/SOURCE.json")
  got=$(cd "$dir" && find . -type f ! -name SOURCE.json | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
  [ "$got" = "$want" ] || { echo "$dir: tree hash $got, SOURCE.json records $want" >&2; exit 1; }
  echo "ok $dir"
done
