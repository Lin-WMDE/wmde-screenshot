name := 'wmde-screenshot'
appid := 'fun.wmde.Screenshot'

rootdir := ''
prefix := '/usr'
cargo-target-dir := env('CARGO_TARGET_DIR', 'target')

base-dir := absolute_path(clean(rootdir / prefix))
bin-dst := base-dir / 'bin' / name
data-dir := base-dir / 'share'

# Baked into the activation file, so without rootdir: it is a path the session bus
# resolves at run time, not a path in the build tree.
bindir := clean(prefix / 'bin')

# Default recipe which runs `just build-release`
[private]
default: build-release

# Compiles with debug profile
build-debug *args:
    cargo build {{args}}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Runs `cargo clean`
clean:
    cargo clean

# Runs a clippy check
check *args:
    cargo clippy --all-features {{args}} -- -W clippy::pedantic

# Runs a clippy check with JSON message format
check-json: (check '--message-format=json')

# Installs files
install:
    install -Dm0755 {{ cargo-target-dir / 'release' / name }} {{bin-dst}}
    sed 's|@bindir@|{{bindir}}|' data/dbus-1/{{appid}}.service.in | install -Dm0644 /dev/stdin {{ data-dir / 'dbus-1' / 'services' / (appid + '.service') }}
    install -Dm0644 data/applications/{{appid}}.desktop {{ data-dir / 'applications' / (appid + '.desktop') }}
    install -Dm0644 data/component-settings/{{appid}}.ron {{ data-dir / 'wmde' / 'component-settings' / (appid + '.ron') }}
