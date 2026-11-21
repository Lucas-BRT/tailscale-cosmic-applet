#!/usr/bin/env sh
set -eu

exec cargo nextest run --locked --workspace "$@"
