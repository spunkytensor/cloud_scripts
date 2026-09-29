#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Matt Curfman
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

# Trivy's Cargo.toml-aware scan omits dev-only crates even with include-dev-deps.
# Scan the resolved lockfile alone as a conservative source/build superset.
root=$(pwd)
report=$(realpath -m "$1")
isolated=$(mktemp -d)
trap 'rm -rf "$isolated"' EXIT
mkdir -p "$report" "$isolated/source"
cp Cargo.lock "$isolated/source/"
cd "$isolated"
trivy fs source --scanners vuln --include-dev-deps --list-all-pkgs \
  --ignorefile /dev/null --ignore-unfixed=false --format json --output "$report/trivy.json"
trivy convert --format spdx-json --output "$report/sbom.spdx.json" "$report/trivy.json"
trivy convert --format cyclonedx --output "$report/sbom.cdx.json" "$report/trivy.json"
trivy version --format json > "$report/trivy-version.json"
git -C "$root" rev-parse HEAD > "$report/source-commit.txt"
(cd "$report" && sha256sum ./*.json source-commit.txt > SHA256SUMS)
python3 "$root/packaging/check-inventory.py" "$root/Cargo.lock" "$report/trivy.json"
jq -e '[.Results[]?.Vulnerabilities[]? | select(.Severity == "HIGH" or .Severity == "CRITICAL")] | length == 0' "$report/trivy.json"
