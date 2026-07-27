# Repository guidance

This is a small Rust control plane for disposable DigitalOcean Codex workers. Keep changes narrow, readable, dependency-light, and consistent with the persisted lifecycle state machine.

## Repository map

- `src/main.rs` dispatches the CLI and renders human/JSON output.
- `src/lifecycle.rs` owns create, pause, resume, destroy, and recovery orchestration.
- `src/backend/` owns provider API behavior.
- `src/state/` owns locking, atomic state persistence, validation, and compatibility import.
- `src/config.rs` owns native TOML configuration.
- `cloud-init.yaml` is embedded into the Rust binary and defines the Ubuntu worker bootstrap.
- `vps.example.toml` is the committed configuration contract.
- `README.md` is the operator guide and must remain consistent with command syntax, prerequisites, lifecycle behavior, security guarantees, and recovery steps.

## Credentials and operator data

- Never print credentials, add DigitalOcean credentials to cloud-init, or write secrets into tracked files.
- Keep GitHub credentials repository-scoped and instance-owned.
- Do not edit or delete `.state/` or `vps.toml`; they are operator data and may contain secrets or ownership evidence.
- If temporary credential files are required, create them with mode `0600` and remove them before finishing.

## Implementation conventions

- Preserve mutation journaling, resumability, and fail-safe teardown semantics.
- On uncertain provider operations, retain enough state to reconcile safely. Never report deletion, revocation, or stopped billing unless the provider confirmed it or the operator supplied the explicit confirmation required by the CLI.
- Validate user-controlled identifiers and paths before using them.
- Write lifecycle and credential state atomically and keep per-instance locking intact.
- Prefer the existing modules and error types over new wrappers or dependencies.
- When changing defaults or accepted settings, update `vps.example.toml`. When changing CLI or operational behavior, update `README.md` in the same change.

## Verification

Run side-effect-free checks from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Do not invoke lifecycle commands as tests: create, pause, resume, and destroy can allocate billable infrastructure, mutate remote hosts, delete resources, and revoke credentials. Use mocks and temporary state for executable lifecycle coverage.
