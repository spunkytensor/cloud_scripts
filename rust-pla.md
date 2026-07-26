# Plan: rewrite the VPS control plane in Rust

## 1. Goal and definition of done

Replace the Bash control plane with a Rust application while preserving its
current safety, recovery, credential-isolation, and billing guarantees. The
operator-facing program becomes one executable named `vps` with subcommands.
DigitalOcean remains the default cloud backend, but cloud-specific behavior is
behind an explicit backend contract and the backend can be selected on every
command.

The rewrite is complete when:

- `vps create`, `vps list`, `vps destroy`, `vps shell`, `vps pause`, and
  `vps resume` provide parity with the current scripts. Pause/resume is existing
  production behavior and may not be deferred from the Rust parity release.
- The implemented cold-pause lifecycle, including worker quiescence, provider
  action reconciliation, private snapshot ownership, replacement recovery,
  and post-recovery snapshot cleanup, is represented in the provider-neutral
  state machine rather than left as DigitalOcean-specific command logic.
- `--backend digitalocean` is accepted wherever a backend is relevant and is
  the default when neither the CLI nor configuration selects one.
- Adding another cloud does not require changes to lifecycle orchestration,
  credential handling, SSH handling, or state storage. At least a fake backend
  is used in tests, and a second production backend must pass the backend
  conformance suite before the project claims multi-cloud production support.
- Existing DigitalOcean instances can be discovered and safely adopted from
  the current `.state/instances/*` files. No active resource becomes orphaned
  merely because the executable was upgraded.
- No tracked user-facing `.sh` control-plane scripts remain after the final
  migration. `cloud-init.yaml` remains YAML; owned bootstrap behavior should
  use cloud-init modules and direct command argument arrays rather than adding
  another large shell program.
- CI runs formatting, linting, unit tests, integration tests, dependency policy,
  and vulnerability/configuration scans.
- Tagged releases produce checksummed archives for macOS and Windows and
  installable Ubuntu `.deb` packages.
- Rust is pinned to **1.96.1**, and every Cargo package uses
  **`edition = "2024"`**. CI must fail if either drifts.

`create` is the canonical spelling. If compatibility with the requested
`creat` spelling is desired, Clap may expose `creat` as a hidden alias, but all
documentation and output should use `vps create`.

## 2. Preserve these existing contracts

The Rust rewrite must retain behavior, not merely reproduce the happy path.
The following are release-blocking invariants.

### Resource and billing safety

- A cloud server remains potentially billable until the selected provider
  confirms deletion or the operator supplies a narrowly scoped explicit
  confirmation.
- An ambiguous create, snapshot, or delete result keeps a durable intent
  journal. It is reconciled before another mutation is attempted.
- A unique provider correlation value is persisted before each non-idempotent
  create operation. DigitalOcean currently uses a lifecycle tag.
- Zero reconciliation matches remains uncertain, one identity-verified match
  may be adopted, and multiple matches fail closed.
- State is never removed while it is the only record of a possibly billable
  server or owned snapshot.
- Teardown must not report stopped billing unless provider state or an explicit
  operator confirmation establishes that result.
- Pause must not delete a source server until a directly retrieved snapshot has
  passed ID, name, source-resource, region, and minimum-disk-size checks and its
  identity is durably recorded.
- Resume must retain the recovery snapshot until the replacement worker's SSH
  identity, containers, repository, GitHub credential, ChatGPT login, Codex
  remote control, and pairing flow have all been verified.
- Destroy must inventory the active, pause-source, and resume-target servers
  plus the owned snapshot from both terminal and transition state. It confirms
  every possible server absent before PAT revocation; if snapshot deletion then
  fails, the PAT remains revoked and snapshot ownership state remains for retry.

### Instance and state isolation

- Each instance has its own validated ID, state directory, lock, SSH trust
  material, repository/base branch, work branch, GitHub credential, and remote
  control marker.
- Instance IDs continue to allow only letters, digits, `.`, `_`, and `-`, and
  may not begin with punctuation.
- One instance may be mutated by only one lifecycle command at a time, while
  different instances may proceed concurrently.
- State directories are mode `0700`; state, journals, tokens, generated SSH
  configuration, and temporary files are mode `0600` on Unix.
- Every durable state change uses a same-directory temporary file, flushes file
  content, atomically renames it, and syncs the containing directory where the
  platform supports it.
- A malformed or contradictory state fails closed. `list` may display it as
  invalid, but mutating commands must not guess.

### Credentials and remote identity

- Cloud API credentials stay on the control machine and are never included in
  cloud-init, uploaded to a worker, written to logs, or serialized into state.
- Every worker receives a separate fine-grained GitHub PAT restricted to its
  selected repository. Its local retained copy exists only to resume and
  revoke safely.
- Interrupted PAT replacement retains both old and new values until the remote
  copy is reconciled and the superseded token is revoked.
- GitHub credentials are not revoked while a possibly running server still
  needs them. They are revoked after all possible servers are confirmed absent,
  even if non-compute cleanup such as snapshot deletion still needs retrying.
- Existing GitHub revocation behavior and the two-day PAT guidance remain.
- SSH uses per-instance known-host data and trust on first use for initial
  creation. Snapshot resume requires the captured host key to match; it must
  not silently fall back to TOFU.
- Root SSH remains disabled and all post-provisioning operations run as the
  unprivileged `agent` user.
- A tracked ChatGPT/Codex enrollment marker is required before reusing an
  existing remote login.
- New workers preserve `/etc/ssh/ssh_host_*` across private-snapshot boots via
  cloud-init `ssh_deletekeys: false`. Pause verifies that configuration before
  quiescing; workers created before this support are refused and must be
  recreated.
- The SSH alias is stable as `codex-vps-<instance-id>` even though server ID and
  public IP change after resume. Initial creation uses TOFU, while resume writes
  the captured Ed25519 key first and uses strict host-key checking.

### Resumability and compatibility

- `create` is both create and resume for an active setup checkpoint.
- The persisted repository and base branch must match on resume before any
  credential or checkout mutation.
- The work branch remains `codex/<instance-id>`.
- A rebooted worker can restart Codex remote control and issue a fresh pairing
  code without allocating a replacement server.
- Destroy supports the existing recovery decisions:
  `--forget-unresolved-allocation`, `--confirm-missing-server` (with a temporary
  compatibility alias for `--confirm-missing-droplet`),
  `--confirm-missing-snapshot`, `--confirm-request-not-accepted`, and
  `--forget-unrevoked-token`. `pause` and `resume` expose only the confirmation
  flags relevant to their current transition.
- A pause or resume rerun continues its existing transition. It never starts a
  second operation, and once destroy records teardown intent, the lifecycle
  command refuses to resume and directs the operator to finish destroy.
- `create` refuses paused, transitional, and contradictory active-plus-paused
  state. `shell` refuses paused and unsafe transitional state but remains
  available during `active-snapshot-cleanup-pending`.
- Existing state is migrated or read compatibly; it is never silently ignored.

## 3. CLI design

Use Clap derive and expose one command:

```text
vps [GLOBAL OPTIONS] <COMMAND>

Global options:
  --backend <NAME>       Cloud backend [default: digitalocean]
  --config <PATH>        Configuration file
  --state-dir <PATH>     Override the instance-state root
  --output <table|json>  Machine-readable output where supported
  -v, --verbose          Increase diagnostic detail without exposing secrets
  --version
  -h, --help

Commands:
  create   Allocate a new instance or resume setup
  list     List locally known instances
  destroy  Reconcile and remove an instance and its credentials
  shell    Open an interactive SSH session
  pause    Quiesce and snapshot an instance, then remove compute
  resume   Restore a paused instance from its snapshot
  status   Show detailed local lifecycle state for one instance
  doctor   Validate configuration, credentials, SSH, and provider access
  completion  Generate shell completion scripts
```

Proposed command forms:

```bash
# DigitalOcean is implicit.
vps create --new example-org/repository --branch main

# Memorable ID; creates if absent and resumes if present.
vps create --instance frontend-a example-org/repository --branch main

# Backend may be explicit and is persisted as part of instance identity.
vps --backend digitalocean create --instance frontend-a \
  example-org/repository --branch main

vps list
vps list --all-backends --output json
vps shell frontend-a
vps pause frontend-a
vps resume frontend-a
vps destroy frontend-a
```

CLI rules:

- Prefer conventional subcommand-local flags over carrying forward script
  names in error messages.
- Keep `OWNER/REPOSITORY,BASE_BRANCH` as a deprecated input form for one
  migration release, but document the less ambiguous repository positional
  plus `--branch` form.
- `--backend` defaults in this order: command line, configuration default,
  then `digitalocean`.
- The backend selected at initial creation is persisted in state. Subsequent
  commands infer it from the instance. Supplying a different backend for an
  existing instance is an error rather than an implicit migration.
- `list` reads all local instances by default, regardless of backend. A backend
  filter narrows output but must not make state disappear from teardown.
- `shell` must not hold the lifecycle lock for the duration of the interactive
  session.
- Human output goes to stdout; diagnostics and progress go to stderr. JSON mode
  emits one stable versioned schema and no progress text on stdout.
- Exit codes are documented: `0` success, `2` CLI/configuration error, `3`
  locally invalid state, `4` uncertain provider result requiring retry or
  confirmation, and `5` remote provisioning/recovery failure.

## 4. Workspace and module layout

Start with a single Cargo package. A multi-crate workspace is unnecessary until
another independently versioned executable or library actually exists.

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
deny.toml
src/
  main.rs
  cli.rs
  error.rs
  config.rs
  secret.rs
  model.rs
  state/
    mod.rs
    lock.rs
    legacy.rs
    atomic.rs
  lifecycle/
    mod.rs
    create.rs
    destroy.rs
    list.rs
    pause.rs
    resume.rs
    recovery.rs
  backend/
    mod.rs
    digitalocean.rs
    fake.rs              # compiled only for tests
  github.rs
  ssh.rs
  worker.rs
  output.rs
assets/
  cloud-init.yaml
tests/
  cli.rs
  lifecycle.rs
  migration.rs
.github/workflows/
  ci.yml
  security.yml
  release.yml
```

Keep orchestration in `lifecycle`, cloud API behavior in `backend`, durable
state mechanics in `state`, and remote machine operations in `worker`. Do not
let a provider implementation manipulate state files or GitHub credentials.

Suggested dependencies, to be confirmed against Rust 1.96.1 and the license
policy before adoption:

- `clap` for CLI parsing and generated completions.
- `serde`, `serde_json`, and `toml` for typed state/configuration.
- `reqwest` with rustls for DigitalOcean and GitHub HTTPS APIs.
- `tokio` for API polling, timeouts, and concurrent read-only operations.
- `thiserror` for typed library errors and `anyhow` only at the binary boundary,
  if needed.
- `tracing` and `tracing-subscriber` for structured, redacted diagnostics.
- `secrecy` plus explicit redacting wrappers for token values.
- `fs2` or a small OS-specific file-lock implementation for advisory locks.
- `tempfile` for safe same-directory temporary state writes in tests and local
  operations.
- `time` for RFC 3339 timestamps and bounded reconciliation windows.
- `uuid` or a cryptographically random URL-safe identifier for operation IDs.
- `zeroize` for credential buffers where practical.
- An SSH implementation selected by a time-boxed spike. Prefer a maintained
  pure-Rust client with Ed25519 known-host verification, ssh-agent support,
  encrypted private-key support, exec, stdin streaming, and interactive PTY.
  If no candidate meets all requirements, isolate an OpenSSH subprocess behind
  `SshClient`; Windows packages must then preflight the Windows OpenSSH Client
  capability and clearly report how to enable it.

Avoid provider CLIs (`doctl`, `aws`, and similar) in the Rust implementation.
Direct APIs provide typed responses, explicit retry control, consistent
cross-platform packaging, and testable HTTP boundaries. Continue to allow the
system `git` and `gh` binaries on the remote worker because they are part of the
environment being provisioned, not local cloud-provider dependencies.

## 5. Backend abstraction

### Contract

Define an object-safe asynchronous `Backend` trait. Keep it at the resource
semantics needed by the lifecycle rather than mirroring DigitalOcean endpoints.
Representative operations are:

```rust
#[async_trait]
pub trait Backend: Send + Sync {
    fn name(&self) -> BackendName;
    fn capabilities(&self) -> Capabilities;

    async fn validate_access(&self) -> Result<AccessReport, BackendError>;
    async fn validate_ssh_key(&self, key: &SshKeyRef) -> Result<PublicKey, BackendError>;

    async fn create_server(&self, request: &CreateServer) -> Result<Mutation<Server>, BackendError>;
    async fn find_servers(&self, correlation: &Correlation) -> Result<Vec<Server>, BackendError>;
    async fn get_server(&self, id: &ResourceId) -> Result<Option<Server>, BackendError>;
    async fn delete_server(&self, id: &ResourceId) -> Result<Mutation<()>, BackendError>;
    async fn public_endpoint(&self, id: &ResourceId) -> Result<Option<Endpoint>, BackendError>;

    async fn shutdown_server(&self, id: &ResourceId) -> Result<Mutation<Action>, BackendError>;
    async fn force_power_off(&self, id: &ResourceId) -> Result<Mutation<Action>, BackendError>;
    async fn create_snapshot(&self, request: &CreateSnapshot) -> Result<Mutation<Action>, BackendError>;
    async fn find_snapshots(&self, correlation: &Correlation) -> Result<Vec<Snapshot>, BackendError>;
    async fn get_snapshot(&self, id: &ResourceId) -> Result<Option<Snapshot>, BackendError>;
    async fn delete_snapshot(&self, id: &ResourceId) -> Result<Mutation<()>, BackendError>;
    async fn get_action(&self, id: &ResourceId) -> Result<Option<Action>, BackendError>;
}
```

The exact Rust signatures may differ, but the design must preserve these
properties:

- Opaque string resource IDs; orchestration must not assume DigitalOcean's
  numeric IDs.
- Typed `Server`, `Snapshot`, `Action`, `Region`, `Size`, and `Image` values.
- Explicit capabilities such as snapshots, graceful shutdown, tags/labels,
  IPv4, and provider action polling.
- Mutations distinguish confirmed success, confirmed rejection, and uncertain
  outcome. An HTTP timeout is not flattened into an ordinary failure.
- Orchestration owns retry and reconciliation policy. A backend must disable
  automatic retries for non-idempotent requests unless the provider supports a
  persisted idempotency key with documented semantics.
- Provider-specific fields are retained in a versioned state extension for
  later identity verification, but lifecycle code consumes normalized fields.
- Backend errors carry a safe diagnostic and structured category/status while
  never carrying request authorization headers or token values.

### DigitalOcean implementation

Implement DigitalOcean first and make it the default:

- Authenticate from `DIGITALOCEAN_TOKEN`, with deprecated support for
  `DOCTL_TOKEN` during migration. Never read or invoke `doctl` authentication.
- Convert `DO_SSH_KEY`, `DO_REGION`, `DO_SIZE`, `DO_IMAGE`, `DO_TAGS`, and
  `DROPLET_NAME_PREFIX` to namespaced TOML settings while retaining environment
  aliases for one migration release.
- Use the v2 REST API for SSH key lookup, Droplet create/get/list/delete,
  actions, shutdown/power-off, snapshot creation/discovery/deletion, sizes,
  regions, and images.
- Send the unique persisted lifecycle tag on create and validate it after a
  successful response as well as after an uncertain response.
- Disable hidden mutation retries. Retry only safe GET/list polling with capped
  exponential backoff plus jitter and an overall deadline.
- Parse API error status and request ID into redacted typed errors.
- Keep the current 404 account-mismatch safeguard; a missing resource alone is
  not sufficient to discard local ownership state.
- Validate the configured public key against the local private/public key pair
  before allocation.

### Additional backends

The initial parity release may ship only `digitalocean`, but it must not claim
that unimplemented names work. `vps --backend <unknown>` should list compiled
backends and fail before touching state. Multi-cloud production support is a
separate release gate:

1. Choose the second backend based on operator demand (for example Hetzner
   Cloud, AWS EC2, or Linode/Akamai).
2. Implement it only in `backend/<name>.rs` plus namespaced configuration.
3. Run the same backend conformance tests using recorded HTTP fixtures and an
   opt-in live test account.
4. Document capability differences. If snapshots or graceful shutdown are not
   supported, `pause` must fail before mutation with a capability error; it
   must not emulate an unsafe partial lifecycle.

This is preferable to implementing several shallow adapters without the
reconciliation and teardown guarantees of the DigitalOcean path.

## 6. Configuration design

Replace executable `config.env` sourcing with non-executable TOML. This removes
command-substitution behavior and makes configuration portable to Windows.

```toml
version = 1
default_backend = "digitalocean"
state_dir = "~/.local/state/vps/instances"

[ssh]
private_key = "~/.ssh/id_digitalocean_v2"

[git]
author_name = "Codex VPS Agent"
author_email = "codex-vps-agent@users.noreply.github.com"

[backends.digitalocean]
ssh_key = "fingerprint-or-id"
region = "nyc3"
size = "s-4vcpu-8gb"
image = "ubuntu-24-04-x64"
tags = ["codex-agent"]
name_prefix = "codex-agent"
```

Configuration precedence is:

1. CLI arguments.
2. Explicit environment variables intended for automation/secrets.
3. TOML configuration.
4. documented defaults.

Rules:

- Default configuration paths follow platform conventions using `directories`:
  XDG paths on Linux, Application Support on macOS, and AppData on Windows.
- `--config` and `VPS_CONFIG` select an explicit file.
- The default state location should remain the repository-local
  `.state/instances` for the migration release when invoked from a checkout
  containing legacy state. New standalone installations should use the
  platform state directory. `vps doctor` reports the effective paths.
- Tokens are environment variables or OS credential-store entries, never TOML
  fields. Backend modules document their own token variable.
- Expand `~` and documented environment placeholders deliberately; do not run
  a shell or support arbitrary command substitution.
- Unknown keys are errors to catch misspellings. Include `version = 1` and
  provide explicit migrations for later schema versions.
- `vps config migrate` may be added to translate non-secret values from
  `config.env`, but it must parse only the simple assignments emitted by the
  committed example. It must never `source` the file.

## 7. Durable state model and migration

### New state

Use one versioned JSON document per instance for ordinary state plus one
versioned JSON write-ahead journal for an in-flight mutation. Keep token bytes
in separate protected files so ordinary state can be inspected and logged
safely.

```text
<state-root>/<instance-id>/
  instance.json
  transition.json              # present only while reconciliation is needed
  github-token
  github-token.replacement
  remote-control.json
  known_hosts
  ssh-config                   # only if the OpenSSH transport is selected
  lifecycle.lock
```

`instance.json` contains:

- Schema version, instance ID, backend name, and creation timestamp.
- Repository, base branch, work branch, and credential-isolation version.
- Terminal lifecycle state: reserved, allocating, active, paused, or
  active-snapshot-cleanup-pending.
- Normalized server identity and provider-specific identity evidence.
- Public endpoint and SSH alias derived from the stable instance ID.
- Snapshot identity when paused or pending cleanup.
- No PAT, cloud token, private key content, or ChatGPT credential.

Model lifecycle phases as enums and validate cross-field invariants during
deserialization. Do not represent contradictory combinations with many
optional fields when a tagged enum can make them impossible.

The Rust model must faithfully represent the currently implemented states:

- Terminal states: `source-reserved`, `allocation-pending`, `active`, and
  `paused`.
- Pause transitions: `pausing-quiescing`, `pausing-shutdown`,
  `pausing-snapshot`, and `pausing-delete-pending`.
- Resume transitions: `resuming-allocation`, `resuming-recovery`, and
  `active-snapshot-cleanup-pending`.
- During resume recovery, active server state, paused snapshot state, and the
  resume transition intentionally coexist. That is valid until snapshot
  cleanup completes. Active-plus-paused state without a matching resume
  transition is contradictory and must be reported as invalid.

Transition data must retain all immutable recipes and mutation checkpoints now
written by `write_transition_state`: operation and pause-operation IDs, start
time, source server identity/endpoint/region/size/tags/disk, quiescence marker,
shutdown and power-off intents/action IDs, snapshot identity and intent/action,
source deletion intent/result, captured host key, target create recipe and
correlation tag, target identity/endpoint, recovery timestamp, snapshot delete
intent/result, and teardown/target-deletion markers. Provider IDs become opaque
strings in the new model, but the legacy DigitalOcean importer validates their
current numeric representation.

### Locking

- Prefer a held OS advisory lock on a dedicated file, recording PID, process
  start metadata, command, and acquisition timestamp for diagnostics.
- Confirm lock semantics on Linux, macOS, and Windows. Tests must use separate
  processes, not only threads.
- Avoid the Bash lock-directory race and stale PID reuse problem. If a platform
  lock is released automatically at process death, no lock stealing is needed.
- `list` reads without acquiring every lifecycle lock, but recognizes an
  atomic, complete prior state and reports transition details when present.

### Legacy migration

Implement a strict, one-way, crash-safe importer for current files:

- Recognize `current.env`, `.setup`, `.allocation`, `.paused`, `.transition`,
  `.github-token`, `.github-token.replacement`, `.remote-control`,
  `.known_hosts`, and `.ssh_config`.
- Parse only the exact `%q`-escaped assignment grammar written by the current
  scripts. Never source legacy files and never invoke Bash to interpret them.
- Validate every imported identifier, repository, branch, path, URL, resource
  ID, and ownership marker.
- Infer `backend = "digitalocean"` for legacy state and preserve the exact
  Droplet ID, lifecycle tag, IP, name, token files, SSH paths, and setup
  checkpoints.
- Import paused snapshot name/ID/source/region/size/tags/minimum-disk evidence,
  captured SSH Ed25519 key, pause operation ID, and paused timestamp. Import
  every pause/resume transition field without collapsing an outstanding intent
  into a completed result.
- Preserve the remote `/home/agent/.config/vps-codex/pause.env` contract until
  Rust has resumed the worker or safely destroyed its resources. It contains
  the pause operation ID, host key, running container IDs, and previously
  healthy container IDs and is part of recovery identity, not disposable
  scratch data.
- Write new state atomically, reread and validate it, and only then record a
  migration-complete marker. Keep legacy files until the operator runs a
  separately documented cleanup command after successful reconciliation.
- If import is ambiguous, stop and explain which field cannot be interpreted.
  Do not allocate, delete, revoke, or rewrite the source files.
- Test migration fixtures for active, allocation-pending, source-reserved,
  paused, all seven pause/resume transition phases, active snapshot cleanup,
  credential-replacement, teardown-started, malformed, contradictory,
  partially written, and orphaned states.
- Before removing Bash entry points, have them print a migration notice and
  delegate only when it is demonstrably safe. Do not maintain two independent
  mutating implementations for an extended period.

## 8. Lifecycle implementation

### Create and resume setup

Implement a durable sequence whose every mutation has a persisted predecessor:

1. Parse and validate CLI/configuration without acquiring credentials unnecessarily.
2. Reserve or validate the instance ID and acquire its lifecycle lock.
3. Persist repository, base branch, work branch, backend, and immutable create
   recipe before cloud mutation.
4. Validate local SSH identity and provider registration.
5. Render cloud-init from an embedded, versioned asset with the authorized
   public key. Do not write it outside a protected temporary file unless the
   provider accepts user data directly from memory.
6. Persist correlation data and create intent.
7. Submit create once. Classify rejection versus uncertain transport/provider
   result.
8. Persist a returned server ID, or reconcile through the unique correlation
   value before any resubmission.
9. Poll the exact server until it has a public endpoint, with a deadline.
10. Persist endpoint state and initialize per-instance host-key trust.
11. Wait for SSH and cloud-init readiness; verify Docker and worker tooling.
12. Create, validate, retain, and install the repository-scoped PAT. Resume the
    existing two-token replacement journal if present.
13. Clone or validate the exact repository, configure commit identity, and
    create/validate `codex/<instance-id>` without overwriting work.
14. Validate ChatGPT login ownership or run the TTY-backed device flow.
15. Start Codex remote control, validate its JSON status, issue a short-lived
    pairing code, and persist non-secret enrollment metadata.
16. Commit active state and print connection and teardown commands using `vps`.

Remote setup currently implemented as a large heredoc should become small,
typed `Worker` operations. Send arguments separately, stream secrets only on
stdin, and never construct shell source by interpolating user values. If the
SSH transport necessarily invokes a remote shell, use a single audited
POSIX-quoting routine with property tests and keep each command small.

Eliminate the tracked `/usr/local/bin/gh` Bash wrapper. Configure `gh` using its
supported token/login flow and `gh auth setup-git`, or install a tiny audited
credential helper implemented by the Rust program if remote-control sessions
cannot otherwise inherit credentials. Verify both ordinary Git and `gh` from a
fresh SSH process.

### List and status

- Remain local and read-only by default; do not query every provider account.
- Include `INSTANCE`, `BACKEND`, `STATUS`, resource ID, IP/endpoint, snapshot
  ID, source repository, and branch.
- Derive status from a valid transition before terminal state.
- Emit explicit invalid/orphaned state instead of reporting active.
- JSON output includes a schema version and stable enum values.
- `status --refresh` may explicitly query the persisted backend/resource and
  show drift without mutating or deleting state.

### Shell

- Require active state or active-snapshot-cleanup-pending.
- Refuse paused and transitional states with the exact recovery command.
- Use the persisted endpoint, `agent` user, selected identity, and per-instance
  known-host policy.
- Allocate a PTY and propagate terminal resize, signals, and child exit status
  on Linux, macOS, and Windows.
- Do not print the private key path in JSON diagnostics unless explicitly
  requested and safe.

### Destroy

Build an inventory from both terminal state and transition journal before
deleting anything:

- Active: stop remote control opportunistically, delete exact server, then
  revoke the PAT.
- Allocation uncertain: reconcile by correlation; delete one verified result,
  fail closed on many, and retain zero-match uncertainty unless explicitly
  forgotten.
- Paused: delete the exact snapshot, then revoke credentials once no server can
  exist.
- Pausing/resuming: reconcile and delete every possible source/target server
  and owned snapshot.
- Active snapshot cleanup pending: delete active server and retained snapshot.
- Unknown deletion or an unexplained 404 preserves resource and credential
  ownership state.
- Local state cleanup happens only after compute deletion, required snapshot
  deletion, and credential revocation are confirmed or separately marked for
  safe retry.

Use provider-neutral flag names in new help text. Retain old DigitalOcean flag
aliases only for migrated DigitalOcean instances.

### Pause and resume

Port the behavior now implemented by `vps_pause.sh`, `vps_resume.sh`, shared
helpers in `lib.sh`, and transitional teardown in `vps_destroy.sh`. The old
`snapshot_plan.md` remains useful design rationale, but executable behavior and
README recovery guarantees are the parity source of truth.

#### Pause parity

1. Acquire the common instance lifecycle lock and reject unresolved fresh
   allocation, already paused state, resume transition, or teardown-started
   transition.
2. Retrieve and identity-check the active server. Record its actual name,
   endpoint, region, size, tags, and disk size rather than profile defaults.
   Reject attached block-storage volumes before remote mutation because the
   boot-disk snapshot does not include them.
3. Verify snapshot-safe cloud-init host-key preservation exists with expected
   ownership/mode. Stop Codex, record running and healthy Docker container IDs,
   gracefully stop running containers with the current 120-second timeout,
   verify they stopped, run `sync`, capture the SSH Ed25519 public key, and
   atomically write the remote pause marker keyed by the pause operation ID.
4. Compare the captured key with the active endpoint's learned key before
   setting the durable quiescence checkpoint.
5. Persist shutdown intent before submitting graceful shutdown with hidden
   retries disabled. Poll and validate exact action ID, type, resource ID, and
   completion. If the response is lost, reconcile actions by type, source ID,
   and transition start time; zero stays uncertain and many fail closed.
6. Independently require provider status `off`. After the bounded graceful
   wait, hard power-off is allowed only because quiescence was verified; its
   intent/action receives the same reconciliation treatment.
7. Persist the globally unique snapshot name and request intent before one
   snapshot submission. Poll the exact action, then discover exactly one
   snapshot matching name, source, and region. Retrieve it directly and verify
   ID, resource type, source, region, and minimum disk size before recording it.
8. Remove stale endpoint trust/config before deleting the source server so a
   recycled IP cannot be trusted. Persist delete intent, confirm exact deletion,
   and treat a 404 as unresolved without `--confirm-missing-server` or the
   DigitalOcean compatibility alias.
9. Atomically commit paused state, remove active state, and remove the
   transition last. Report that compute billing stopped while snapshot storage
   remains billable.

#### Resume parity

1. Require setup ownership and paused state, or continue an existing resume
   transition. Reject active state without a matching cleanup transition,
   fresh allocation state, pause transition, and teardown-started state.
2. Retrieve and revalidate the exact private snapshot. Persist a replacement
   recipe using the paused server's name, region, size, tags, source disk size,
   pause operation ID, and captured host key. Do not rerun cloud-init.
3. Persist one unique target correlation tag and create intent. Create from the
   snapshot with the configured SSH key, reconcile the target by tag even after
   an apparently successful response, and require exactly one match. Persist
   target ID and endpoint and verify ID, name, image, region, size, and tag.
4. Write known-host data containing the captured key before connection,
   atomically replace active endpoint state/config, and use strict host-key
   checking. Verify the remote Ed25519 key again after SSH is available.
5. Read the remote marker matching the pause operation ID. Start only containers
   that were running, verify each is running, and wait for containers previously
   marked healthy to become healthy again. Containers stopped before pause stay
   stopped.
6. Verify worker-ready/cloud-init history, Docker access, repository origin,
   exact work branch, repository ownership marker, retained PAT equality, and
   `gh` authentication. Resume or initiate the protected two-PAT replacement
   workflow if the short-lived PAT expired while paused.
7. Require the tracked ChatGPT ownership marker and surviving ChatGPT login,
   restart Codex remote control, validate connected/connecting daemon status,
   and obtain a fresh pairing code.
8. Persist `RECOVERY_VERIFIED_AT` and enter
   `active-snapshot-cleanup-pending` before attempting snapshot deletion.
   `shell` remains available in this phase, but another pause is refused.
9. Persist snapshot-delete intent, revalidate exact snapshot identity on retry,
   delete it, and require confirmation or `--confirm-missing-snapshot`. Remove
   paused state, transition state, and the remote marker only after cleanup.

#### Cross-command recovery

- `--confirm-request-not-accepted` clears only a specific unresolved action,
  snapshot, or replacement-create intent after independent operator
  confirmation; it never skips identity or recovery verification.
- `list` must expose the exact transition phases and snapshot ID locally,
  report invalid transition and contradictory terminal states explicitly, and
  never perform provider reads implicitly.
- `destroy` sets teardown intent in the transition before cleanup, reconciles an
  unidentified target by correlation tag and an unidentified requested
  snapshot by name/source/region, deduplicates resource IDs, and refuses partial
  teardown on multiple matches.
- Snapshot deletion occurs only after every possible server is confirmed absent
  and PAT revocation has been attempted. A failed snapshot deletion retains
  paused/transition ownership state for retry while leaving the credential
  revoked to protect the stranded image.
- Each phase must document and test its entry invariant, observable completion
  condition, safe retry, valid confirmation flag, shell behavior, and destroy
  path.
- Treat snapshots as credential-bearing resources and never share or publish
  them.

## 9. Error, retry, and secret handling

- Define typed errors for CLI/config, state, lock, backend rejection, uncertain
  mutation, SSH trust, remote command, GitHub, timeout, and unsupported
  capability.
- Attach remediation commands to errors as structured data so table and JSON
  output remain consistent.
- Redact values by type, not by post-processing log strings. `SecretString`
  must not implement `Display`; debug output is redacted.
- Configure `reqwest` so authorization headers are never logged. Tracing spans
  may contain provider name, operation ID, resource ID, HTTP status, and request
  ID, but no bodies known to contain credentials.
- Safe reads use capped exponential backoff with jitter. Mutation retries are
  forbidden unless an accepted provider idempotency key was persisted before
  the first request.
- Every poll has a deadline and reports the checkpoint retained for retry.
- Ctrl-C and termination stop at a durable boundary. Signal handling should not
  attempt complicated cleanup that could erase evidence of an uncertain call.

## 10. Testing strategy

All default tests must be side-effect-free and must never contact a real cloud,
create a server, revoke a real credential, or modify operator state.

### Unit tests

Test at minimum:

- CLI parsing, defaults, aliases, conflicts, exit codes, and JSON output.
- TOML precedence, unknown keys, path expansion, and secret exclusion.
- Instance ID, repository, branch, provider resource, and URL validation.
- State serialization round trips, schema rejection, enum invariants, and
  provider extension preservation.
- Atomic-write behavior and permissions on Unix; replacement semantics on
  Windows.
- Cross-process lock contention and release after process death.
- Legacy `%q` parser with adversarial values; prove it never executes input.
- Secret redaction in all errors, debug output, tracing, and HTTP failures.
- Retry classification for 4xx, 408, 429, 5xx, timeout, malformed JSON, and
  connection loss.
- Remote argument quoting with spaces, quotes, newlines, metacharacters, and
  Unicode.
- DigitalOcean request/response mapping and exact identity checks.
- GitHub PAT validation, install, replacement journal, and revocation ordering.

Use property tests where they buy confidence: state round trips, legacy parsing,
argument quoting, identifier validation, and redaction.

### Backend conformance tests

Every backend runs the same contract suite against a deterministic fake HTTP
server:

- Confirmed create and delete.
- Confirmed provider rejection.
- Response lost after provider accepted create/delete.
- Zero, one, and multiple correlation matches.
- Returned resource with wrong name, image, region, size, or correlation.
- Delayed endpoint and delayed resource visibility.
- 404/account mismatch behavior.
- Action pending, completed, errored, malformed, and wrong-resource cases.
- Snapshot discovery and capability refusal.
- No automatic retry of non-idempotent requests.

Store sanitized fixtures without tokens, account IDs, public IPs, or private
repository names.

### Lifecycle integration tests

Run the real orchestration with fake backend, fake SSH/worker, fake GitHub, a
temporary state root, and an injectable clock/random source. Cover:

- Fresh create and resumed create from every persisted boundary.
- State-write failure before and after provider acceptance.
- IP polling and cloud-init/SSH timeouts.
- Repository mismatch and dirty/wrong work branch.
- PAT creation, expiry, interrupted replacement, foreign remote token, and
  revocation failure.
- ChatGPT marker mismatch and Codex start/pair failures.
- Normal destroy and each uncertainty/confirmation path.
- Pause/resume interruption at all seven implemented transition phases,
  including every request-intent/result sub-checkpoint within each phase.
- Paused-state snapshot validation, pre-snapshot worker refusal, attached-volume
  refusal, source deletion after durable snapshot verification, strict resumed
  host key, remote container inventory matching, and snapshot cleanup retry.
- `list` output and `shell` refusal for paused, contradictory, malformed, and
  transitional local states; shell success during cleanup pending.
- Destroy from every create, pause, and resume phase.
- Independent instance concurrency.

Use table-driven state-machine tests that enumerate every phase and assert:

1. The safe next action.
2. Whether mutation is allowed.
3. Which resources remain owned.
4. Whether credentials may be revoked.
5. What `list`, `status`, `shell`, and `destroy` must do.

### CLI and packaging smoke tests

- Run the built binary on Ubuntu, macOS, and Windows for `--version`, `help`,
  `completion`, `doctor` with intentionally missing credentials, and JSON
  output snapshots.
- Install and remove each `.deb` in a clean Ubuntu container/VM and verify file
  ownership, executable mode, completions/man page, and no undeclared runtime
  library requirement.
- Extract macOS/Windows archives and verify checksum, executable name, version,
  and architecture.
- Keep real-provider tests opt-in, manually approved, budget-limited, tagged,
  and followed by an independent cleanup job. They are not pull-request gates.

## 11. CI

Pin the Rust toolchain in `rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.96.1"
profile = "minimal"
components = ["rustfmt", "clippy"]
```

Set `rust-version = "1.96.1"` and `edition = "2024"` in `Cargo.toml`. Commit
`Cargo.lock`. Add a CI assertion that reads Cargo metadata and the toolchain
file so a dependency or package cannot quietly raise the MSRV or change the
edition.

### Pull-request CI (`ci.yml`)

Run on pull requests, pushes to the default branch, and manual dispatch:

1. **Formatting:** `cargo fmt --all -- --check`.
2. **Lint:** `cargo clippy --workspace --all-targets --all-features --locked --
   -D warnings`.
3. **Tests:** `cargo test --workspace --all-features --locked` on Ubuntu,
   macOS, and Windows.
4. **Minimal/default features:** test default features separately if provider
   feature flags are introduced.
5. **Docs:** `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
   --all-features --locked`.
6. **MSRV/toolchain:** all jobs use exactly 1.96.1; do not use floating stable.
7. **Dependency cache:** cache Cargo registry/git data and target artifacts with
   keys containing OS, target, Cargo.lock hash, and toolchain. Never cache
   credentials or local state.

Use least-privilege workflow permissions (`contents: read` by default), pin
third-party actions to full commit SHAs, set job timeouts, cancel superseded PR
runs, and do not expose secrets to forked pull requests.

### Security CI (`security.yml`)

Run on pull requests when dependency/policy files change, on the default branch,
weekly, and manually:

- `cargo deny check` using a pinned cargo-deny release.
- Trivy repository filesystem/configuration/secret scan.
- Trivy vulnerability scan of release filesystem artifacts and generated SBOM.
- Upload SARIF to GitHub code scanning only with the minimum required
  `security-events: write` permission and only in trusted contexts.
- Retain human-readable reports as CI artifacts, with secret values redacted.

Renovate or Dependabot may propose Rust crate and GitHub Action updates, but
updates remain subject to the pinned toolchain, cargo-deny, tests, and Trivy.

## 12. Cargo Deny policy

Add a commented `deny.toml` and treat every exception as a reviewed item with
owner, rationale, and expiry/removal condition.

### Advisories

- Deny known vulnerabilities, unmaintained crates, unsound crates, and yanked
  crates by default.
- Keep the RustSec advisory database fetched in CI; do not rely on an old cache
  for scheduled runs.
- Ignore an advisory only when it is demonstrably unreachable or no fixed
  version exists and risk is accepted. Record the advisory ID, reason, issue,
  and review date beside the exception.

### Licenses

- Use confidence threshold at least `0.93`.
- Start with a narrow allowlist suitable for binary distribution, for example
  Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0, and Zlib.
- Review MPL-2.0 or other weak-copyleft dependencies individually before
  allowing them. Deny GPL/AGPL and unknown/unlicensed code unless the project's
  distribution policy explicitly changes.
- Generate a third-party license notice for release archives and packages from
  the resolved lockfile.

### Bans and sources

- Warn on duplicate versions initially, then deny avoidable duplicates after a
  baseline cleanup. Explicitly skip only versions that cannot yet converge.
- Deny wildcard dependencies.
- Deny crates known to undermine the selected TLS/crypto policy or duplicate
  an approved stack without justification.
- Deny git dependencies and unknown registries by default. Allow only
  crates.io's sparse index unless a specific source is reviewed and pinned to
  an immutable revision.
- Deny unapproved native TLS/OpenSSL dependencies if the selected implementation
  is intended to use rustls; verify this with `cargo tree` in CI.
- Check all feature combinations shipped in release artifacts, not only the
  default feature graph.

## 13. Trivy scanning

Pin Trivy by version and verify the downloaded binary/checksum or use a
SHA-pinned official action. Configure:

### Repository scan

Run `trivy fs` from the repository root with:

- Vulnerability scanning for lockfiles and vendored/package content.
- Misconfiguration scanning for GitHub Actions, cloud-init, Docker-related
  configuration, and package metadata.
- Secret scanning for committed credentials and high-confidence key patterns.
- `HIGH,CRITICAL` vulnerability failure threshold, including unfixed findings
  unless a time-limited reviewed exception exists.
- A committed `.trivyignore.yaml` only for advisory IDs with rationale and
  expiration; never use a blanket ignore file or `--exit-code 0` in the gating
  job.

### Artifact and SBOM scan

- Generate a CycloneDX JSON SBOM for each release target from the final staged
  package contents.
- Scan the staged macOS archive tree, Windows archive tree, and extracted `.deb`
  filesystem, not just source.
- Scan the SBOM with the same severity policy and upload it with the release.
- Ensure debug symbols, local state, test fixtures containing synthetic secret
  patterns, `.env` files, and CI credentials are absent from packages.

Trivy and cargo-deny overlap intentionally: Cargo Deny enforces Rust advisory,
license, source, and duplicate policy; Trivy catches non-Rust artifacts,
workflow/configuration issues, secrets, OS-package issues, and release-content
drift.

## 14. Release, cross-compilation, and packaging

### Supported artifacts

Produce at least:

| OS | Target | Artifact |
|---|---|---|
| macOS | `aarch64-apple-darwin` | `vps-<version>-aarch64-apple-darwin.tar.gz` |
| Windows | `x86_64-pc-windows-msvc` | `vps-<version>-x86_64-pc-windows-msvc.zip` |
| Windows | `aarch64-pc-windows-msvc` | archive after SSH/PTY support is verified; otherwise document x64-only initially |
| Ubuntu | `x86_64-unknown-linux-gnu` | `vps_<version>_amd64.deb` |

macOS x64 and Ubuntu arm64 packaging are explicitly disabled in the current
release matrix. Re-enabling either target requires restoring its native build,
smoke-test, SBOM, and staged-filesystem scan job rather than publishing an
untested cross-compiled artifact.

Build macOS artifacts on GitHub-hosted macOS and Windows MSVC artifacts on
Windows. Build `.deb` binaries on the oldest supported Ubuntu/glibc baseline or
in a pinned builder container. Do not claim compatibility based only on a newer
runner. Prefer rustls and pure-Rust dependencies to minimize system runtime
libraries.

Each archive/package includes:

- `vps` or `vps.exe`.
- README and license.
- Third-party license notice.
- Shell completions and a generated man page where applicable.
- No configuration containing credentials and no local `.state` data.

The Debian package installs the binary to `/usr/bin/vps`, man page under
`/usr/share/man`, completions under distribution-appropriate paths, and license
documents under `/usr/share/doc/vps`. Package scripts should be absent unless a
real installation need is identified.

### Release workflow (`release.yml`)

Trigger only for a signed/approved `v*` tag and manual dry runs:

1. Check that tag version, Cargo package version, and generated CLI version
   match.
2. Run the full CI and security gates from the tagged commit.
3. Build each target with `--locked --release` using Rust 1.96.1.
4. Strip safely where supported while retaining separate debug artifacts only
   if the project chooses to publish them.
5. Stage deterministic archive contents and normalize timestamps where tools
   allow.
6. Build `.deb` packages with explicit architecture, dependencies, metadata,
   and file modes.
7. Install/smoke-test artifacts on their native OS/architecture.
8. Generate CycloneDX SBOMs and third-party notices.
9. Run Trivy against staged/extracted artifacts and SBOMs.
10. Generate SHA-256 checksums for every published file.
11. Produce provenance/attestations using GitHub artifact attestations with
    job-scoped `id-token: write` and `attestations: write` only where needed.
12. Upload immutable workflow artifacts, then publish one GitHub Release after
    every matrix job succeeds.

Do not use `cargo install` during release to obtain floating tool versions.
Pin packaging, SBOM, cargo-deny, and Trivy tool versions and record them in the
workflow or a checked-in tools manifest.

Code signing/notarization should be a documented follow-up gate:

- macOS artifacts should eventually be Developer ID signed and notarized.
- Windows artifacts should eventually be Authenticode signed.
- Release workflows must still produce checksums and attestations before those
  credentials are available, and must never expose signing secrets to PR jobs.

## 15. Documentation changes during implementation

Rewrite `README.md` alongside behavior changes, not after the old scripts are
deleted. It must document:

- Installation from archives and `.deb`, plus source builds pinned to 1.96.1.
- `vps` command syntax and the `create` spelling.
- Backend selection, DigitalOcean default, supported backend capabilities, and
  namespaced credentials/settings.
- Migration from `config.env`, old script commands, and legacy `.state`.
- Current DigitalOcean scopes and direct API token setup without `doctl`.
- GitHub PAT and ChatGPT device-login interaction.
- Cost, teardown, ambiguous API recovery, and every confirmation/forget flag.
- SSH trust model and Windows OpenSSH requirements if the subprocess transport
  is selected.
- Pause/resume cold-boot semantics, changed server ID/IP, snapshot billing,
  credential-bearing images, volume limitations, and cleanup-pending behavior.
- CI, supported targets, checksums, SBOMs, attestations, cargo-deny policy, and
  Trivy exceptions.

Keep an architecture decision record for the SSH transport and a backend-author
guide documenting the trait, capability model, conformance tests, mutation
rules, configuration namespace, and redaction requirements.

## 16. Implementation sequence and release gates

### Phase 0: decisions and executable specification

- Record current CLI outputs and create sanitized legacy-state fixtures.
- Convert current safety behavior into the lifecycle/state-machine tests.
- Complete the SSH transport spike on Linux, macOS, and Windows.
- Confirm all selected crate versions compile on Rust 1.96.1 and pass the
  initial license/source policy.
- Decide the standalone default config/state locations and migration behavior.

**Gate:** no provider mutation code begins until uncertain outcomes, state
atomicity, SSH capability, and migration parsing have testable designs.

### Phase 1: Rust skeleton and read-only functionality

- Add pinned toolchain, Cargo package, CLI, config, typed errors, output, and
  redaction.
- Implement atomic state, locking, typed model, and strict legacy importer.
- Implement `vps list`, `vps status`, `vps doctor`, completions, and version.
- Add baseline CI, cargo-deny, and Trivy repository scans.

**Gate:** read-only commands correctly represent every current and planned
state, and malformed legacy input cannot execute code.

### Phase 2: complete DigitalOcean lifecycle parity

- Implement direct DigitalOcean API client and fake backend conformance suite.
- Implement SSH/worker and GitHub services.
- Implement create/resume setup, list, shell, pause, snapshot resume, and
  destroy with durable journals.
- Import and continue every current paused and transitional state before the
  corresponding Bash command is retired.
- Preserve snapshot-safe cloud-init, stable SSH aliasing, strict resumed host
  identity, remote container inventory/recovery, PAT replacement during a long
  pause, and final snapshot-cleanup-pending shell access.
- Test interruption at every mutation boundary and teardown from every create,
  pause, and resume phase.
- Run opt-in live smoke tests with a dedicated low-privilege account, strict
  budget, unique tags, and independent cleanup audit.

**Gate:** Rust can safely adopt, pause, resume, and destroy active, paused, or
transitional instances created by the Bash implementation, and no parity
invariant in section 2 is missing.

### Phase 3: pause/resume backend conformance and resilience

- Run the backend-neutral pause/resume state machine against deterministic fake
  backends with and without snapshots, action polling, tags, graceful shutdown,
  and attached-volume reporting.
- Prove unsupported capabilities fail before worker quiescence or provider
  mutation.
- Add exhaustive worker-recovery tests for host-key continuity, running/stopped
  and healthy/unhealthy containers, missing remote marker, expired PAT,
  repository drift, ChatGPT ownership, Codex restart, and pairing failure.
- Perform protected DigitalOcean live tests for shutdown, forced power-off only
  after quiescence, snapshot identity, restore, changed endpoint, preserved host
  key, container recovery, PAT replacement, snapshot cleanup, and destroy from
  interrupted phases.

**Gate:** a failed pause/resume or destroy never leaves an untracked billable
server or credential-bearing snapshot, and adding a backend cannot bypass the
same orchestration invariants.

### Phase 4: operator migration

- Ship `vps` beside scripts for one migration release.
- Make Bash scripts non-authoritative wrappers that print the equivalent `vps`
  command only after complete lifecycle parity is proven; avoid split ownership
  of active, paused, or transitional state.
- Provide `vps config migrate`, `vps doctor`, state migration preview, and
  rollback instructions that preserve legacy files.
- Update README and support documentation.

**Gate:** migration has been tested against copies of every legacy state shape,
including interrupted allocation, paused state, every pause/resume transition,
active snapshot cleanup, teardown-started state, and token replacement. No
cleanup command deletes legacy evidence by default.

### Phase 5: packaging and Rust-only cutover

- Add native target builds, `.deb`, checksums, SBOM, Trivy artifact scans,
  provenance, and release publication.
- Remove obsolete Bash control-plane files only after a stable migration
  release and explicit review of remaining cloud-init behavior.
- Move cloud-init to `assets/`, embed it in the binary, and remove dependencies
  on repository-relative runtime files.

**Gate:** clean machines can install one artifact, run `vps doctor`, adopt or
create an instance, shell into it, and safely destroy it.

### Phase 6: second production backend

- Select the provider based on real demand.
- Implement only its adapter and configuration namespace.
- Pass conformance, lifecycle, teardown, documentation, security, and opt-in
  live tests.

**Gate:** only then advertise the project as production multi-cloud rather than
multi-backend-ready with DigitalOcean support.

## 17. Final acceptance checklist

- [ ] Only one operator-facing executable, `vps`, is required.
- [ ] `vps create`, `list`, `status`, `shell`, `destroy`, `pause`, and `resume`
      have documented behavior and tests.
- [ ] DigitalOcean is the default and `--backend digitalocean` is explicit and
      persisted.
- [ ] Backend orchestration has no DigitalOcean-specific IDs, terminology, or
      retry assumptions.
- [ ] Existing active, paused, cleanup-pending, and interrupted Bash state
      imports without shell evaluation and remains recoverable.
- [ ] Every current pause/resume transition, mutation checkpoint, recovery flag,
      stable SSH identity rule, container recovery rule, and transitional
      destroy path has parity coverage before the Bash command is retired.
- [ ] Unknown cloud outcomes retain a journal and never cause blind retries.
- [ ] State writes, file permissions, locks, token replacement, SSH trust, and
      credential revocation preserve current guarantees on all supported OSes.
- [ ] Rust 1.96.1 and edition 2024 are pinned and enforced.
- [ ] Formatting, Clippy, tests, docs, Cargo Deny, and Trivy gate pull requests.
- [ ] Cargo Deny covers advisories, licenses, bans, duplicates, and sources with
      reviewed, expiring exceptions only.
- [ ] Trivy scans source/config/secrets, release filesystems, and SBOMs and fails
      on unapproved high/critical findings.
- [ ] macOS arm64, Windows x64, and Ubuntu amd64 artifacts are built and
      smoke-tested; macOS x64, Ubuntu arm64, and Windows arm64 remain explicitly
      deferred until their native release jobs are restored and verified.
- [ ] Ubuntu `.deb` installation/removal is tested on the supported baseline.
- [ ] Releases include checksums, SBOMs, third-party notices, and provenance.
- [ ] Documentation accurately describes backend support, prerequisites,
      migration, billing, credentials, recovery, and security limitations.
- [ ] No test or CI job can allocate billable infrastructure without an
      explicit protected opt-in and an independent cleanup path.
