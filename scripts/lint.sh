#!/usr/bin/env sh
set -eu

exec cargo clippy --locked --workspace --all-targets "$@" -- -D warnings
