#!/usr/bin/env bash
set -euo pipefail

target=${1:?target required}
: "${2:?version required}"
dest=${3:?destination required}
binary="target/${target}/release/vps"

[[ -x "$binary" ]] || { echo "missing binary: $binary" >&2; exit 1; }
[[ -f README.md ]] || { echo "README.md is required release input" >&2; exit 1; }
rm -rf "$dest"
mkdir -p "$dest/bin" "$dest/completions" "$dest/man"
install -m 0755 "$binary" "$dest/bin/vps"
install -m 0644 README.md vps.example.toml "$dest/"
[[ ! -f LICENSE ]] || install -m 0644 LICENSE "$dest/"
install -m 0644 target/THIRD_PARTY_LICENSES.txt "$dest/"
"$binary" completion bash >"$dest/completions/vps.bash"
"$binary" completion zsh >"$dest/completions/_vps"
"$binary" completion fish >"$dest/completions/vps.fish"
"$binary" man >"$dest/man/vps.1"
find "$dest" -type f -exec touch -d '@0' {} + 2>/dev/null || true
