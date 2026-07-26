#!/usr/bin/env bash
set -euo pipefail
stage=${1:?staged tree required}
version=${2:?version required}
arch=${3:?Debian architecture required}
out=${4:?output required}
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/man/man1" \
  "$root/usr/share/bash-completion/completions" "$root/usr/share/zsh/vendor-completions" \
  "$root/usr/share/fish/vendor_completions.d" "$root/usr/share/doc/vps"
install -m 0755 "$stage/bin/vps" "$root/usr/bin/vps"
install -m 0644 "$stage/man/vps.1" "$root/usr/share/man/man1/vps.1"
install -m 0644 "$stage/completions/vps.bash" "$root/usr/share/bash-completion/completions/vps"
install -m 0644 "$stage/completions/_vps" "$root/usr/share/zsh/vendor-completions/_vps"
install -m 0644 "$stage/completions/vps.fish" "$root/usr/share/fish/vendor_completions.d/vps.fish"
install -m 0644 "$stage/README.md" "$root/usr/share/doc/vps/README.md"
install -m 0644 "$stage/vps.example.toml" "$root/usr/share/doc/vps/vps.example.toml"
install -m 0644 "$stage/THIRD_PARTY_LICENSES.txt" "$root/usr/share/doc/vps/"
[[ ! -f "$stage/LICENSE" ]] || install -m 0644 "$stage/LICENSE" "$root/usr/share/doc/vps/"
cat >"$root/DEBIAN/control" <<EOF
Package: vps
Version: $version
Architecture: $arch
Maintainer: cloud_scripts maintainers
Section: admin
Priority: optional
Description: Safe control plane for disposable cloud workers
EOF
find "$root" -print0 | xargs -0 touch -d '@0'
dpkg-deb --root-owner-group --build "$root" "$out"
