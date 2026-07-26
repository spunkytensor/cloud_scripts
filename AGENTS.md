# Repository guidance

This is a small Bash control plane for disposable DigitalOcean Codex workers. Keep changes narrow, readable, and dependency-light; do not introduce a framework or build system for work that can remain in the existing scripts.

## Repository map

- `lib.sh` owns shared configuration, instance-state, locking, SSH, logging, and error helpers.
- `vps_create.sh`, `vps_destroy.sh`, `vps_list.sh`, and `vps_shell.sh` are the user-facing commands.
- `cloud-init.yaml` defines the remote Ubuntu worker and its bootstrap scripts.
- `config.example.env` is the committed configuration contract. `config.env` and `.state/` are local, ignored, and may contain secrets.
- `README.md` is the operator guide and must stay consistent with command syntax, prerequisites, lifecycle behavior, security guarantees, and recovery steps.

## Environment-provided credentials

The development environment may provide these values as environment variables:

- `DOCTL_SSH_KEY` — ID or fingerprint of the SSH key installed at DigitalOcean for VPS access.
- `DOCTL_TOKEN` — token used to authenticate DigitalOcean API requests.
- `SSH_DIGITAL_OCEAN` — private SSH key corresponding to the installed DigitalOcean key.
- `SSH_DIGITAL_OCEAN_PUB` — matching public SSH key.

Use these variables when credentials are needed instead of asking for or inventing values. Never print their contents, include them in command output, write them into tracked files, or copy them to a VPS. If a command requires key files, materialize them only in protected temporary files and remove those files when finished.

## Implementation conventions

- Target Bash, not POSIX `sh`. Preserve `#!/usr/bin/env bash` and `set -euo pipefail` in executable scripts.
- Quote expansions unless splitting or globbing is deliberate. Prefer `[[ ... ]]`, Bash arrays, `local` variables, and the existing `die`, `log`, and `require_command` helpers.
- Put behavior shared by multiple commands in `lib.sh`; keep command-specific lifecycle logic in its command.
- Validate user-controlled identifiers and paths before using them. Pass remote command arguments with `remote_exec` or explicit positional parameters rather than interpolating them into shell source.
- Write lifecycle and credential state atomically through mode-`0600` temporary files followed by `mv`. Keep per-instance directories mode `0700`.
- Preserve resumability and fail-safe teardown semantics. On an uncertain cloud operation, retain enough state to reconcile safely; never report deletion, revocation, or stopped billing unless the provider confirmed it or the operator supplied an existing explicit confirmation flag.
- Never print credentials or add DigitalOcean credentials to VPS provisioning. Keep GitHub credentials repository-scoped and instance-owned.
- Avoid editing or deleting `.state/` and `config.env`; they are operator data, not fixtures.
- When changing defaults or accepted settings, update `config.example.env`. When changing the CLI or operational behavior, update `README.md` in the same change.

## Verification

Run local, side-effect-free checks from the repository root:

```bash
bash -n lib.sh vps_create.sh vps_destroy.sh vps_list.sh vps_shell.sh
shellcheck lib.sh vps_create.sh vps_destroy.sh vps_list.sh vps_shell.sh
```

If `shellcheck` is unavailable, report that and still run `bash -n`. Do not invoke `vps_create.sh` or `vps_destroy.sh` merely as a test: they can allocate billable infrastructure, mutate remote hosts, and revoke credentials. For lifecycle changes, reason through fresh creation, interrupted/resumed creation, normal teardown, API uncertainty, and retry paths; use mocks or isolated temporary state if executable coverage is added.
