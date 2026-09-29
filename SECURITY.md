# Security policy

## Reporting vulnerabilities

Do not post credentials, private snapshot contents, or exploit details in public
issues. GitHub private vulnerability reporting was **disabled** when checked on
2026-09-29. No working private contact has been confirmed. The maintainer
(@spunkytensor) must enable and test private reporting before claiming this gap is
closed. If the repository's Security tab offers **Report a vulnerability**, use
that private form; otherwise request a private contact without disclosing the
vulnerability. Revoke exposed credentials immediately.

## Supported releases

The maintainer is @spunkytensor. The current development line is `main` (`0.1.0`);
no GitHub releases were published as of 2026-09-29 and no historical release line
has a security-support commitment. Fixes target `main`; users must review changes
before updating. Release support and end dates must be declared before shipping.

## Spunky Tensor security

This repository adopts the [shared baseline](https://github.com/spunkytensor/.github/blob/69b5f260fb4358acb0e2f7b2a96254ad9cc2322c/docs/baseline.md)
at commit `69b5f260fb4358acb0e2f7b2a96254ad9cc2322c`, with incomplete adoption
explicitly recorded below. `.github/workflows/public-repo-security.yml` runs on
every PR, `main` push, and nightly at **09:49 UTC** (01:49 PST / 02:49 PDT).
Trivy 0.74.0 scans resolved source dependencies,
retains all severities, and blocks High/Critical findings even without fixes.
Scanner failures and empty inventories fail. The inventory job additionally
scans an isolated `Cargo.lock` and requires every external package name/version
to be present. This supplements the shared Cargo.toml-aware scan, which omitted
five dev-only crates during validation despite `--include-dev-deps`. The isolated
inventory is a conservative superset, not a runtime-only dependency claim.

Existing cargo-deny license/advisory/source policy, Trivy 0.72.0 vulnerability,
secret and configuration checks, release SBOM scans, signed-tag checks, and build
provenance remain. No suppressions are added. Findings need a maintainer-assigned
owner and remediation date. Exceptions require package/version/finding scope,
evidence, owner, reviewer, expiry, and tracking reference; the shared scan does
not implement waivers.

## SBOM downloads and release evidence

PR/nightly [Actions runs](https://github.com/spunkytensor/cloud_scripts/actions/workflows/public-repo-security.yml)
retain `security-source` for 30 days: `sbom.spdx.json`, `sbom.cdx.json`,
`trivy.json`, tool metadata, source identity, and checksums. Failed scans can leave
partial evidence. `security-lockfile` adds both SBOM formats, full-lockfile scan,
tool/DB metadata and source commit for the same 30 days. These are diagnostics,
not permanent release downloads.

The existing release pipeline builds an Apple Silicon macOS tarball, Windows x64
ZIP, and Linux amd64 Debian package. On a verified signed release tag, it publishes
both SBOM formats, Trivy reports, third-party notices/license texts, per-target
source/artifact digest metadata, checksums, and provenance alongside the packages
in [GitHub Releases](https://github.com/spunkytensor/cloud_scripts/releases).
Target-prefixed `.cdx.json` and `.spdx.json` are **source-derived inventories**,
not proof of everything linked into each native binary. Shared source evidence
has a `source-` filename prefix (full-lock evidence uses `lockfile-`); internal checksums use
the original artifact filenames. Nothing is published from pull requests.

## Remaining adoption gaps

- **Administration:** private reporting is disabled. Dependency graph, Dependabot
  alerts/security updates, secret scanning/push protection, CodeQL availability,
  required checks/reviews, code-owner enforcement, maintainer 2FA and access review
  need administrator verification. Branch protection inspection returned HTTP 403.
  This rollout changes no settings. Rust CodeQL integration remains outstanding;
  dependency review is configured for PRs and requires dependency graph availability.
- **Freshness:** the central report can discover the caller path above, but its
  deployment and >36-hour stale-scan alert delivery need verification. No successful
  nightly run is claimed before merge. GitHub schedules are best effort and can
  disable after 60 days of inactivity.
- **Distributed inventory:** reconcile actual native linkage, toolchain/runtime
  components and target-specific build output before claiming complete platform
  SBOMs. There are currently no published artifacts to rescan. Before the first
  release, register immutable supported artifact digests and nightly rescanning;
  scanning `main` does not scan an older downloaded binary. No public container is
  shipped, so a container caller with a fictitious digest is inappropriate.
- **Worker software:** `cloud-init.yaml` is embedded, but Ubuntu packages, Docker,
  NodeSource Node 22 and the remotely downloaded Codex installer are obtained on
  operator machines at provisioning time. Their versions, OS/runtime CVEs, installer
  provenance and licenses are not covered by the Rust inventory. Private snapshots
  and user repositories are not release artifacts and must never be uploaded for
  public scanning. No provisioning is performed by these checks.
- **Legal/distribution:** see [third-party notices](THIRD_PARTY_NOTICES.txt).
  cargo-about remains the attribution generator; an ORT/ScanCode review and review
  of upstream copyright/NOTICE preservation, native dependencies, source-delivery
  obligations, and logo provenance are outstanding. macOS signing/notarization and
  Windows signing are not provided. Existing provenance is not a claimed SLSA level.
