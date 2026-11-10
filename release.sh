#!/bin/bash
# Usage: ./release.sh 0.2.0
# Bumps the version (commit), tags it, then pushes master and the tag: the docker workflow builds the image of
# master (latest) then, for the tag, reuses its cache (same commit) and publishes nbittich/file-search:<version>.
set -euo pipefail
if [ $# -ne 1 ]; then
  echo "usage: $0 <version>" >&2
  exit 1
fi
cargo release --no-tag --no-push --no-publish --dependent-version upgrade --execute "$1"
git tag "$1"
git push
git push origin "$1"
