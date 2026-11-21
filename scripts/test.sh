#!/usr/bin/env sh
set -eu

exec cargo test --locked --workspace "$@"
