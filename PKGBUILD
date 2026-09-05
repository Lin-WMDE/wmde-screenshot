# Maintainer: WMDE <https://wmde.fun>
#
# Builds our own Lin-WMDE/wmde-screenshot (branch wmde). No upstream: COSMIC has no
# screenshot client, so this one is written here and never merged from anywhere. The
# capture half moves in from wmde-portal, which keeps only the portal interface.
#
# The program is a resident service on fun.wmde.Screenshot, started by the session bus
# through the activation file the first time something asks for a shot.
pkgname=wmde-screenshot
pkgver=0.1.0
pkgrel=1
pkgdesc="WMDE screenshot tool - owns fun.wmde.Screenshot"
arch=('x86_64')
url="https://wmde.fun"
license=('GPL-3.0-only')
# runtime: wayland client (libcosmic winit/wayland). Vulkan is reached through dlopen,
# so makepkg cannot see it in ldd - the loader and a driver come from wmde-desktop.
depends=('glibc' 'gcc-libs' 'wayland')
makedepends=('rust' 'cargo' 'just' 'git' 'wayland' 'clang' 'lld' 'pkgconf')
source=("$pkgname::git+https://github.com/Lin-WMDE/wmde-screenshot.git#branch=wmde")
sha256sums=('SKIP')

pkgver() {
  cd "$srcdir/$pkgname"
  # WMDE version: <COSMIC major.minor>.<WMDE release>.<commits since the nearest tag>.g<short>.
  local desc
  desc=$(git describe --long --tags --abbrev=7 2>/dev/null || true)
  if [ -n "$desc" ]; then
    printf '1.7.2.%s.g%s' "$(printf '%s' "$desc" | sed -E 's/.*-([0-9]+)-g[0-9a-f]+$/\1/')" "$(git rev-parse --short=7 HEAD)"
  else
    printf '1.7.2.%s.g%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
  fi
}

build() {
  cd "$srcdir/$pkgname"
  # x86-64-v3 (AVX2/BMI2) baseline for the WMDE repo; runs on Haswell+ (and the VM).
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu=x86-64-v3"
  just build-release
}

check() {
  cd "$srcdir/$pkgname"
  # The dev profile on purpose: the release profile aborts on panic, and the test harness
  # needs to unwind to report a failure.
  cargo test --locked 2>&1 | tail -30
}

package() {
  cd "$srcdir/$pkgname"
  # installs /usr/bin/wmde-screenshot and the D-Bus activation file
  just rootdir="$pkgdir" prefix=/usr install
  # The toolbar's own icon set. One style, one source: the glyphs used to come from five
  # different themes, half of them not symbolic and so unable to take the button's colour.
  find data/icons -type f | while read -r f; do
    install -Dm644 "$f" "$pkgdir/usr/share/icons/hicolor/${f#data/icons/}"
  done

  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
