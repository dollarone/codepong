#!/usr/bin/env bash

set -euo pipefail

GIT_COMMIT=$(git rev-parse main)
GIT_COMMIT_SHORT=$(echo "$GIT_COMMIT" | cut -b -8)
DOCKER_TAG="codepong:latest"

mkdir -p app_packages

git archive --format=tar "$GIT_COMMIT" | sudo docker build -t "$DOCKER_TAG" --build-arg "git_version=$GIT_COMMIT" -

sudo docker run --rm "$DOCKER_TAG" \
tar -c \
app \
codepong \
handlebars \
static \
| gzip > "app_packages/codepong_$GIT_COMMIT_SHORT.tar.gz"

sudo docker build -f app_package_Dockerfile -t generic_app_host:latest .
