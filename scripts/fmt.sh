#!/usr/bin/env sh
set -eu

exec cargo fmt --check "$@"
