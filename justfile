name := 'tailscale-cosmic-applet'
appid := 'io.github.lucasbrt.TailscaleCosmicApplet'
rootdir := ''
prefix := '/usr'
local-prefix := env('HOME') / '.local'

appdata := appid + '.metainfo.xml'
desktop := appid + '.desktop'

# Installation paths
base-dir := absolute_path(clean(rootdir / prefix))
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
metainfo-dst := base-dir / 'share' / 'metainfo' / appdata
legacy-appdata-dst := base-dir / 'share' / 'appdata' / appdata
bin-dst := base-dir / 'bin' / name
desktop-dst := base-dir / 'share' / 'applications' / desktop
icon-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'scalable' / 'apps' / appid + '.svg'
icon-symbolic-dst := base-dir / 'share' / 'icons' / 'hicolor' / 'symbolic' / 'apps' / appid + '-symbolic.svg'

# Aliases for fast dev workflows
alias d := dev
alias reinstall-dev := dev
alias reinstall := reinstall-release
alias reinstall-test := reinstall-release
alias install-test := install-local
alias uninstall-test := uninstall-local

# Default recipe: build release
default: build-release

# --- Dev shortcuts (fast local iteration, no root needed) ---

# Fast dev cycle: debug build, symlink into ~/.local, refresh assets, and restart applet
dev *args: (build-debug args)
    just prefix="{{local-prefix}}" _install-dev
    just restart

# Release dev cycle: release build, install into ~/.local, and restart applet
reinstall-release *args: (build-release args)
    just prefix="{{local-prefix}}" install
    just restart

# Build debug and link/install into ~/.local without restarting
install-dev *args: (build-debug args)
    just prefix="{{local-prefix}}" _install-dev

# Build release and install into ~/.local without restarting
install-local *args: (build-release args)
    just prefix="{{local-prefix}}" install

# Remove local ~/.local installation
uninstall-local:
    just prefix="{{local-prefix}}" uninstall

# Stop running applet process so cosmic-panel respawns it cleanly
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

# --- Build, test & lint ---

# Compile with debug profile
build-debug *args:
    cargo build --bin {{name}} {{args}}

# Compile with release profile
build-release *args:
    cargo build --release --bin {{name}} {{args}}

# Compile release profile with vendored dependencies
build-vendored *args: vendor-extract (build-release '--frozen --offline' args)

# Run unit tests
test *args:
    cargo test {{args}}

# Run clippy across all targets
check *args:
    cargo clippy --all-targets --all-features {{args}} -- -D warnings

# Run clippy with JSON output
check-json: (check '--message-format=json')

# Format Rust sources
fmt:
    cargo fmt

# Run the applet directly (release)
run *args:
    env RUST_BACKTRACE=full cargo run --release --bin {{name}} {{args}}

# Run the applet directly (debug)
run-debug *args:
    env RUST_BACKTRACE=full cargo run --bin {{name}} {{args}}

# --- System & staging installation ---

# Install release binary and desktop assets to `prefix` (respects `rootdir`)
install: _install-assets
    rm -f "{{bin-dst}}"
    install -Dm0755 "{{ cargo-target-dir / 'release' / name }}" "{{bin-dst}}"
    just prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

# Internal: install assets + symlink debug binary (avoids copying large debug binary on every dev rebuild)
_install-dev: _install-assets
    mkdir -p "$(dirname '{{bin-dst}}')"
    ln -sfn "{{ absolute_path(cargo-target-dir / 'debug' / name) }}" "{{bin-dst}}"
    just prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

# Internal: install desktop entry, metainfo and icons
_install-assets:
    install -Dm0644 "target/xdgen/app.desktop" "{{desktop-dst}}"
    install -Dm0644 "target/xdgen/app.metainfo.xml" "{{metainfo-dst}}"
    install -Dm0644 "resources/icon.svg" "{{icon-dst}}"
    install -Dm0644 "resources/icon-symbolic.svg" "{{icon-symbolic-dst}}"

# Internal: refresh desktop/icon caches when installing directly (not into staged rootdir)
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

# Uninstall installed files (including legacy appdata path if present)
uninstall:
    rm -f "{{bin-dst}}" "{{desktop-dst}}" "{{metainfo-dst}}" "{{legacy-appdata-dst}}" "{{icon-dst}}" "{{icon-symbolic-dst}}"
    just prefix="{{prefix}}" rootdir="{{rootdir}}" _refresh-caches

# --- Cleanup & packaging ---

# Run `cargo clean`
clean:
    cargo clean

# Remove vendored dependencies
clean-vendor:
    rm -rf .cargo vendor vendor.tar

# `cargo clean` and remove vendored dependencies
clean-dist: clean clean-vendor

# Vendor dependencies locally into vendor.tar
vendor:
    mkdir -p .cargo
    cargo vendor --sync Cargo.toml | head -n -1 > .cargo/config.toml
    echo 'directory = "vendor"' >> .cargo/config.toml
    echo >> .cargo/config.toml
    tar pcf vendor.tar vendor .cargo/config.toml
    rm -rf .cargo vendor

# Extract vendored dependencies
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar

# Bump cargo version, create git commit, and create tag
tag version:
    find -type f -name Cargo.toml -exec sed -i '0,/^version/s/^version.*/version = \"{{version}}\"/' '{}' \; -exec git add '{}' \;
    cargo check
    cargo clean
    git add Cargo.lock
    git commit -m 'release: {{version}}'
    git tag -a {{version}} -m ''
