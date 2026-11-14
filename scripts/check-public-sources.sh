#!/usr/bin/env bash
# This repository is public (CLAUDE.md): a copied-in file must not come from a private repository.
# Every `SOURCE.json` (the record of a vendored copy) must name, in `repo`, a GitHub repository that
# the unauthenticated API reports as public. A private repository answers 404 there.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
status=0
while IFS= read -r -d '' file; do
  repo=$(jq -r '.repo // empty' "$root/$file")
  if [ -z "$repo" ]; then
    echo "$file: no \"repo\"" >&2; status=1; continue
  fi
  private=$(curl -fsS -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$repo" | jq -r '.private') || private=unknown
  if [ "$private" != false ]; then
    # The name is not printed: the log of this public repository is public too.
    echo "$file: \"repo\" is not a public repository" >&2; status=1
  fi
done < <(git -C "$root" ls-files -z --cached --others --exclude-standard -- '*SOURCE.json')
[ "$status" = 0 ] && echo "ok public sources"
exit "$status"
