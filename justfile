# Task runner for tailscale-cosmic-applet. Run `just` to list recipes, `just help` for options.

set shell := ['bash', '-euo', 'pipefail', '-c']

name := 'tailscale-cosmic-applet'
appid := 'io.github.lucasbrt.TailscaleCosmicApplet'
rootdir := ''
prefix := '/usr'
local-prefix := env('HOME') / '.local'
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')

appdata := appid + '.metainfo.xml'
desktop := appid + '.desktop'

base-dir := absolute_path(clean(rootdir / prefix))
metainfo-dst := base-dir / 'share' / 'metainfo' / appdata
legacy-appdata-dst := base-dir / 'share' / 'appdata' / appdata
bin-dst := base-dir / 'bin' / name
desktop-dst := base-dir / 'share' / 'applications' / desktop
icon-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps' / appid + '.svg'
icon-symbolic-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'symbolic' / 'apps' / appid + '-symbolic.svg'


[private]
default:
    @just --list

[group('help')]
[doc('Show overridable variables and usage examples')]
help:
    #!/usr/bin/env sh
    cat <<'HELP'
    Variables (override with `just name=value <recipe>`):
      prefix        install prefix for system install        (default: /usr)
      rootdir       staging root prepended to prefix         (default: empty)
      local-prefix  user install prefix                      (default: ~/.local)
      cargo-target-dir  cargo target directory               (default: target)

    Extra arguments are forwarded to cargo, e.g.:
      just build-release --features foo
      just test -- some_test_name
      just clippy --message-format=short

    Typical workflows:
      just dev                 debug build, link into ~/.local, restart applet
      just install-local       release build, install into ~/.local
      sudo just install        release build, install system-wide
      just check               full pre-push gate (fmt, clippy, tests)
    HELP

# --- Development (no root, ~/.local) ---

[group('dev')]
[doc('Debug build, link into ~/.local, refresh assets and restart applet')]
dev *args: (build-debug args)
    {{just_executable()}} prefix="{{local-prefix}}" _install-dev
    {{just_executable()}} restart

[group('dev')]
[doc('Release build, install into ~/.local and restart applet')]
reinstall-release *args: (build-release args)
    {{just_executable()}} prefix="{{local-prefix}}" install
    {{just_executable()}} restart

[group('dev')]
[doc('Debug build and link into ~/.local without restarting')]
install-dev *args: (build-debug args)
    {{just_executable()}} prefix="{{local-prefix}}" _install-dev

[group('dev')]
[doc('Release build and install into ~/.local without restarting')]
install-local *args: (build-release args)
    {{just_executable()}} prefix="{{local-prefix}}" install

[group('dev')]
[doc('Remove the ~/.local installation')]
[confirm('Remove the ~/.local installation? (y/N)')]
uninstall-local:
    {{just_executable()}} prefix="{{local-prefix}}" uninstall

[group('dev')]
[doc('Stop the running applet so cosmic-panel respawns it')]
restart:
    #!/usr/bin/env sh
    set -eu
    killed=0
    for exe in /proc/[0-9]*/exe; do
        target="$(readlink "$exe" 2>/dev/null || true)"
        case "$target" in
            */{{name}}|"*/{{name}} (deleted)")
                pid="${exe#/proc/}"
                pid="${pid%/exe}"
                kill -TERM "$pid" 2>/dev/null && killed=1 || true
                ;;
        esac
    done
    if [ "$killed" -eq 1 ]; then
        echo "Restarted {{name}} (cosmic-panel will respawn it)."
    else
        echo "No running {{name}} process found."
    fi

# --- Build ---

[group('build')]
[doc('Compile with the debug profile')]
build-debug *args:
    cargo build --bin {{name}} {{args}}

[group('build')]
[doc('Compile with the release profile')]
build-release *args:
    cargo build --release --bin {{name}} {{args}}

[group('build')]
[doc('Compile the release profile offline from vendor.tar')]
build-vendored *args: vendor-extract
    cargo build --release --frozen --offline --bin {{name}} {{args}}

[group('run')]
[doc('Run the applet directly (release)')]
run *args:
    RUST_BACKTRACE=full cargo run --release --bin {{name}} {{args}}

[group('run')]
[doc('Run the applet directly (debug)')]
run-debug *args:
    RUST_BACKTRACE=full cargo run --bin {{name}} {{args}}

# --- Quality ---

[group('quality')]
[doc('Run the full pre-push gate: fmt-check, lint, test (scripts/check.sh)')]
check:
    ./scripts/check.sh

[group('quality')]
[doc('Run unit tests via scripts/test.sh; extra args forwarded')]
test *args:
    ./scripts/test.sh {{args}}

[group('quality')]
[doc('Run lint via scripts/lint.sh (clippy, -D warnings)')]
lint *args:
    ./scripts/lint.sh {{args}}

[group('quality')]
[doc('Clippy across all targets and features, with -D warnings')]
clippy *args:
    cargo clippy --all-targets --all-features {{args}} -- -D warnings

[group('quality')]
[doc('Clippy with JSON message output')]
clippy-json: (clippy '--message-format=json')

[group('quality')]
[doc('Format Rust sources in place')]
fmt *args:
    cargo fmt {{args}}

[group('quality')]
[doc('Check formatting without writing (scripts/fmt.sh)')]
fmt-check *args:
    ./scripts/fmt.sh {{args}}

# --- System install (sudo) ---

[group('install')]
[doc('Install release binary and desktop assets to prefix (respects rootdir)')]
install: _require-build _install-assets
    rm -f "{{bin-dst}}"
    install -Dm0755 "{{cargo-target-dir / 'release' / name}}" "{{bin-dst}}"
    {{just_executable()}} prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

[group('install')]
[doc('Remove installed files from prefix (respects rootdir)')]
[confirm('Remove system-wide installed files? (y/N)')]
uninstall:
    rm -f "{{bin-dst}}" "{{desktop-dst}}" "{{metainfo-dst}}" "{{legacy-appdata-dst}}" "{{icon-dst}}" "{{icon-symbolic-dst}}"
    {{just_executable()}} prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

# --- Packaging & release ---

[group('release')]
[doc('Vendor dependencies into vendor.tar')]
vendor:
    mkdir -p .cargo
    cargo vendor --sync Cargo.toml | head -n -1 > .cargo/config.toml
    echo 'directory = "vendor"' >> .cargo/config.toml
    echo >> .cargo/config.toml
    tar pcf vendor.tar vendor .cargo/config.toml
    rm -rf .cargo vendor

[group('release')]
[doc('Extract vendor.tar into ./vendor')]
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar

[group('release')]
[doc('Bump version in Cargo.toml, commit, and create annotated tag')]
[confirm('Bump version, commit and tag? (y/N)')]
tag version:
    find . -type f -name Cargo.toml -exec sed -i '0,/^version/s/^version.*/version = \"{{version}}\"/' '{}' \; -exec git add '{}' \;
    cargo check
    cargo clean
    git add Cargo.lock
    git commit -m 'release: {{version}}'
    git tag -a {{version}} -m ''

# --- Clean ---

[group('clean')]
[doc('Run cargo clean')]
clean:
    cargo clean

[group('clean')]
[doc('Remove vendored dependencies')]
clean-vendor:
    rm -rf .cargo vendor vendor.tar

[group('clean')]
[doc('Run cargo clean and remove vendored dependencies')]
clean-dist: clean clean-vendor

# --- Internal helpers (hidden from the list) ---

[private]
_require-build:
    #!/usr/bin/env sh
    if [ ! -f "target/xdgen/app.desktop" ] || [ ! -f "{{cargo-target-dir / 'release' / name}}" ]; then
        echo "Missing build artifacts. Run 'just build-release' as your user before 'sudo just install'." >&2
        exit 1
    fi

[private]
_install-dev: _install-assets
    mkdir -p "$(dirname '{{bin-dst}}')"
    ln -sfn "{{absolute_path(cargo-target-dir / 'debug' / name)}}" "{{bin-dst}}"
    {{just_executable()}} prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

[private]
_install-assets:
    install -Dm0644 "target/xdgen/app.desktop" "{{desktop-dst}}"
    install -Dm0644 "target/xdgen/app.metainfo.xml" "{{metainfo-dst}}"
    install -Dm0644 "resources/icon.svg" "{{icon-dst}}"
    install -Dm0644 "resources/icon-symbolic.svg" "{{icon-symbolic-dst}}"

[private]
_refresh-caches:
    #!/usr/bin/env sh
    if [ -n "{{rootdir}}" ]; then
        exit 0
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f "{{base-dir}}/share/icons/hicolor" 2>/dev/null || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "{{base-dir}}/share/applications" 2>/dev/null || true
    fi
