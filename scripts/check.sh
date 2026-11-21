#!/usr/bin/env sh
set -eu

dir="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
"$dir/fmt.sh"
"$dir/lint.sh"
"$dir/test.sh"
