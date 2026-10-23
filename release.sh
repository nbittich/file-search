#!/bin/bash
set -e
cargo release  --no-tag  --no-push  --no-publish --dependent-version  upgrade --execute $1
git tag $1
git push origin $1
git push
