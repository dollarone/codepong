#!/usr/bin/env bash

set -euo pipefail

tar -xf "$1"

exec ./app
