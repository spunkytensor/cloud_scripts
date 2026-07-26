# VPS snapshot pause and resume plan

## Objective

Add two lifecycle commands:

```bash
./vps_pause.sh --instance frontend-a
./vps_resume.sh --instance frontend-a
```

`vps_pause.sh` should gracefully quiesce an active worker, snapshot it, and
destroy its Droplet so DigitalOcean compute billing stops. The instance remains
known locally and appears as `paused` in `vps_list.sh`.

`vps_resume.sh` should create a replacement Droplet from that snapshot,
recover the worker, update its local SSH endpoint, and delete the snapshot once
the replacement is fully verified.

This is a cold suspend, not a RAM-level virtual-machine suspend. The snapshot
preserves disk state, including:

- The Git checkout and uncommitted work
- Docker images, containers, and named volumes
- Local Supabase/Postgres data stored on the boot disk
- Installed tools and system configuration
- The repository-scoped GitHub credential
- The ChatGPT/Codex login

Running processes and memory are not preserved. Resume boots a new Droplet from
the snapshotted disk and restarts the required services.

## Cost model

DigitalOcean currently bills CPU Droplets per second, with a minimum charge of
60 seconds or $0.01. Compute billing ends only when the Droplet is destroyed;
powering it off is not sufficient.

A paused instance continues to incur snapshot storage charges. DigitalOcean
currently charges $0.06 per GB per month for Droplet snapshots, with a possible
$0.01 minimum charge.

References:

- [Droplet pricing](https://docs.digitalocean.com/products/droplets/details/pricing/)
- [Snapshot pricing](https://docs.digitalocean.com/products/snapshots/details/pricing/)
- [Creating Droplet snapshots](https://docs.digitalocean.com/products/snapshots/how-to/snapshot-droplets/)

## Operator experience

```bash
./vps_pause.sh --instance frontend-a
./vps_list.sh
./vps_resume.sh --instance frontend-a
./vps_shell.sh frontend-a
```

Expected list states:

```text
INSTANCE     STATUS                  DROPLET_ID  IP             SNAPSHOT_ID  SOURCE
frontend-a   active                  587692369   203.0.113.10   -            mattcurf/frontend@main
frontend-a   pausing-snapshot        587692369   -              pending      mattcurf/frontend@main
frontend-a   paused                  -           -              123456789    mattcurf/frontend@main
frontend-a   resuming-recovery       598712345   203.0.113.42   123456789    mattcurf/frontend@main
frontend-a   active                  598712345   203.0.113.42   -            mattcurf/frontend@main
```

The public IP address and Droplet ID change during resume. This is expected.
`vps_shell.sh frontend-a` reads the latest instance state and hides both changes
from the operator.

The generated SSH alias should also become stable per instance, for example
`codex-vps-frontend-a`, instead of being derived from the replaceable Droplet
ID. This improves direct OpenSSH and Codex Desktop behavior in addition to the
abstraction already provided by `vps_shell.sh`.

## Lifecycle state machine

```text
active
  -> pausing-quiescing
  -> pausing-shutdown
  -> pausing-snapshot
  -> pausing-delete-pending
  -> paused
  -> resuming-allocation
  -> resuming-recovery
  -> active-snapshot-cleanup-pending
  -> active
```

An interruption leaves the instance in its current phase. Rerunning the same
command resumes that phase. It must never create another snapshot or Droplet
without first reconciling the previous request.

## State representation

### Active state

Continue using:

```text
.state/instances/<instance>/current.env
```

This remains the canonical active-Droplet state.

### Paused state

Add:

```text
.state/instances/<instance>/current.env.paused
```

Suggested fields:

```bash
PAUSED_STATE_VERSION=1
SNAPSHOT_ID=123456789
SNAPSHOT_NAME=vps-codex-frontend-a-587692369-...
SNAPSHOT_SOURCE_DROPLET_ID=587692369
SNAPSHOT_CREATED_AT=...
DROPLET_NAME=codex-agent-frontend-a
DROPLET_REGION=nyc3
DROPLET_SIZE=s-4vcpu-8gb
DROPLET_TAGS=codex-agent
SSH_HOST_ED25519_PUBLIC_KEY=...
PAUSED_AT=...
```

The effective region, size, name, and tags must be captured from the actual
Droplet. Resume must not silently adopt profile defaults that changed while the
instance was paused.

### Transition journal

Add one mode-`0600` write-ahead journal:

```text
.state/instances/<instance>/current.env.transition
```

Common fields:

```bash
TRANSITION_VERSION=1
TRANSITION_KIND=pause|resume
TRANSITION_PHASE=...
OPERATION_ID=...
TRANSITION_STARTED_AT=...
```

Pause fields include:

```bash
SOURCE_DROPLET_ID=...
SOURCE_DROPLET_NAME=...
SOURCE_DROPLET_IP=...
SOURCE_REGION=...
SOURCE_SIZE=...
REMOTE_QUIESCE_VERIFIED=0|1
SHUTDOWN_REQUESTED=0|1
SHUTDOWN_ACTION_ID=...
POWER_OFF_REQUESTED=0|1
POWER_OFF_ACTION_ID=...
SNAPSHOT_NAME=...
SNAPSHOT_REQUESTED=0|1
SNAPSHOT_ACTION_ID=...
SNAPSHOT_ID=...
SOURCE_DELETE_REQUESTED=0|1
SOURCE_DELETE_CONFIRMED=0|1
```

Resume fields include:

```bash
SNAPSHOT_ID=...
TARGET_DROPLET_NAME=...
TARGET_REGION=...
TARGET_SIZE=...
TARGET_TAGS=...
TARGET_LIFECYCLE_TAG=...
TARGET_CREATE_REQUESTED=0|1
TARGET_DROPLET_ID=...
TARGET_DROPLET_IP=...
RECOVERY_VERIFIED_AT=...
SNAPSHOT_DELETE_REQUESTED=0|1
SNAPSHOT_DELETE_CONFIRMED=0|1
```

All state writes must use same-directory temporary files, mode `0600`, shell
escaping, and atomic `mv`. The existing per-instance lifecycle lock protects
create, pause, resume, and destroy operations for that instance.

## Provider mutation protocol

Every non-idempotent DigitalOcean operation must follow this protocol:

1. Atomically persist request intent and immutable correlation data.
2. Submit the request once with `doctl --http-retry-max 0`.
3. Validate the response and persist any returned action or resource ID.
4. On a timeout, malformed response, network loss, or interruption, reconcile
   the provider state before considering another request.
5. Continue only when exactly one identity-verified result exists.

Read-only polling may continue to use normal retries.

Asynchronous action completion must be verified from action JSON. Do not assume
that `--wait` returning successfully is sufficient without checking the final
action status, type, and resource ID.

## `vps_pause.sh`

### 1. Validate the active instance

Under the existing instance lifecycle lock:

- Require a valid `current.env` active state.
- Resume an existing pause transition instead of beginning another one.
- Refuse an unresolved creation or resume transition.
- Verify that the exact recorded Droplet still exists.
- Record its actual ID, name, IP, region, size, and tags.
- Reject attached DigitalOcean block-storage volumes in the initial version.
- Capture the Droplet's SSH Ed25519 host public key for resume verification.

Droplet snapshots capture the boot disk. They do not automatically snapshot
attached DigitalOcean volumes or GPU scratch disks. Docker and Supabase volumes
stored on the boot disk are included.

### 2. Quiesce the worker

DigitalOcean recommends powering off a Droplet before snapshotting because a
live snapshot can compromise database consistency.

Before provider shutdown:

1. Stop Codex remote control so the agent stops modifying the workspace.
2. Record which Docker containers are currently running and which were healthy.
3. Gracefully stop those containers with a generous timeout.
4. Verify all recorded containers stopped.
5. Run `sync`.
6. Save a remote pause marker containing the operation ID, SSH host key, and
   container restart inventory.
7. Persist `REMOTE_QUIESCE_VERIFIED=1` locally.

Stopping Docker workloads explicitly provides a clean PostgreSQL/Supabase
consistency boundary. Containers that were already stopped should remain
stopped after resume.

Pause must document that it should not run while an operator is independently
mutating the machine over SSH. The lifecycle lock cannot prevent an unrelated
interactive shell from writing to disk.

### 3. Gracefully shut down the Droplet

Use:

```bash
doctl compute droplet-action shutdown <droplet-id>
```

Then:

- Persist and poll the exact action ID.
- Require the expected action type, resource ID, and `completed` status.
- Independently verify that the Droplet reaches provider status `off`.

If graceful shutdown times out, use `power-off` only after remote container
quiescence and `sync` succeeded. DigitalOcean documents `power-off` as a hard
shutdown and recommends graceful shutdown first.

If a shutdown response is ambiguous but the Droplet is now off, the observable
postcondition resolves it. Otherwise reconcile actions by source Droplet, type,
and operation time window. Never choose arbitrarily among multiple candidates.

The powered-off Droplet remains billable during snapshot creation.

### 4. Create and discover the snapshot

Generate a globally unique snapshot name containing the instance ID, source
Droplet ID, and random operation ID:

```text
vps-codex-frontend-a-587692369-<operation-id>
```

Create it with:

```bash
doctl compute droplet-action snapshot \
  <droplet-id> \
  --snapshot-name <name>
```

The snapshot action returns an action ID rather than the resulting snapshot ID.
After the action explicitly completes:

1. List Droplet snapshots.
2. Match the exact snapshot name.
3. Match `resource_type == droplet`.
4. Match `resource_id` to the source Droplet ID.
5. Match the expected region and creation window.
6. Require exactly one match.
7. Persist the numeric snapshot ID.
8. Retrieve that snapshot directly and revalidate its identity and minimum disk
   size.

If the snapshot request response is lost, reconcile both the source Droplet's
snapshot actions and snapshots by exact name/source. Never submit another
snapshot merely because visibility is delayed.

### 5. Delete the source Droplet and commit paused state

Only after the snapshot ID is durably recorded and verified:

1. Remove or neutralize the old generated SSH endpoint files.
2. Persist `SOURCE_DELETE_REQUESTED=1`.
3. Delete the exact source Droplet ID.
4. Confirm deletion.
5. Atomically write `current.env.paused`.
6. Remove `current.env`.
7. Remove the transition journal last.

If deletion fails, retain the state as `pausing-delete-pending`. Rerunning
`vps_pause.sh` retries only deletion and does not create another snapshot.

If interruption occurs after provider deletion but before the paused-state
commit, the journal owns the verified snapshot and source identity, so a rerun
can safely finish the commit.

A DigitalOcean 404 retains the existing account-mismatch safeguard and requires
explicit `--confirm-missing-droplet` handling.

## `vps_resume.sh`

### 1. Validate the paused snapshot

Under the same lifecycle lock:

- Require `current.env.paused`, or resume an existing resume journal.
- Retrieve the exact snapshot ID.
- Verify its name, source Droplet ID, resource type, region availability, and
  minimum disk size.
- Use the recorded source region and size, not current profile defaults.
- Generate and persist a fresh target lifecycle tag.

### 2. Create the replacement Droplet

Create from the snapshot with:

```bash
doctl compute droplet create <name> \
  --image <snapshot-id> \
  --region <recorded-region> \
  --size <recorded-size> \
  --ssh-keys <configured-key> \
  --tag-names <base-tags>,<new-lifecycle-tag> \
  --enable-monitoring
```

Do not provide `cloud-init.yaml`. The snapshot already contains the initialized
OS, user, Docker installation, Codex installation, repository, and credentials.

Before submitting:

- Persist `TARGET_CREATE_REQUESTED=1`.
- Persist the unique lifecycle tag and immutable create recipe.
- Disable hidden mutation retries with `--http-retry-max 0`.

Reconcile an uncertain create request through the lifecycle tag:

- Zero matches: remain uncertain.
- One identity-verified match: adopt it.
- Multiple matches: fail closed and require manual cleanup.

Even after a successful create response, verify that exactly one Droplet carries
the expected lifecycle tag.

### 3. Replace endpoint and SSH state

After DigitalOcean assigns the new public IP:

1. Persist the new Droplet ID and IP in the transition journal.
2. Verify provider name, image, region, size, and lifecycle tag.
3. Compare the resumed host's SSH Ed25519 public key to the key captured before
   pause.
4. Atomically replace the instance's dedicated known-hosts file.
5. Atomically regenerate the SSH config with the new IP and stable per-instance
   alias.
6. Write the new active `current.env` while retaining the paused state and
   transition until recovery is complete.

A snapshot should preserve `/etc/ssh/ssh_host_*`. A host-key mismatch means the
replacement is not demonstrably the snapshotted machine and must fail before
credential or workspace mutation. Resume should not silently fall back to
trust-on-first-use.

`vps_shell.sh <instance>` continues reading `current.env`, so callers do not
need to know the changed IP, Droplet ID, or alias.

### 4. Recover services, workspace, and remote control

Refactor the post-allocation portion of `vps_create.sh` into shared functions in
`lib.sh`, used by fresh create and snapshot resume. Do not recursively invoke
`vps_create.sh`, and do not duplicate the credential and Codex recovery logic.

The shared recovery path should:

1. Wait for SSH.
2. Verify that cloud-init previously completed and the worker-ready marker
   exists.
3. Verify Docker socket access.
4. Restart containers recorded in the remote pause marker.
5. Require previously running containers to run again.
6. Wait for containers that were healthy before pause to become healthy.
7. Verify repository origin, checkout, and `codex/<instance-id>` branch.
8. Reconcile the retained GitHub token and replacement journal.
9. Prompt for a replacement PAT if the two-day credential expired while paused.
10. Verify the tracked ChatGPT login and ownership marker.
11. Restart Codex remote control.
12. Generate and print a fresh pairing code.

Successful recovery requires all of those checks. Merely receiving an IP or
opening SSH is not sufficient.

### 5. Delete the recovery snapshot

Do not delete the snapshot merely because the replacement Droplet was created.
Delete it only after the replacement worker is fully verified.

After successful recovery:

1. Persist `RECOVERY_VERIFIED_AT`.
2. Commit active state while retaining snapshot ownership in the paused/journal
state.
3. Persist `SNAPSHOT_DELETE_REQUESTED=1`.
4. Delete the exact snapshot ID.
5. Confirm deletion.
6. Remove `current.env.paused`.
7. Remove the remote pause marker.
8. Remove the transition journal last.

If snapshot deletion fails:

- Keep the replacement Droplet active.
- Keep the snapshot ownership state.
- Report `active-snapshot-cleanup-pending`.
- Permit `vps_shell.sh`.
- Make rerunning `vps_resume.sh` retry cleanup only.
- Refuse another pause until snapshot cleanup is resolved.

Deleting the snapshot earlier could leave neither the source Droplet nor a
recovery image after a failed workspace, credential, or container recovery.

## Integration changes

### `lib.sh`

Add shared helpers for:

- Atomic paused-state and transition-state writes
- Strict paused and transition loading/validation
- Stable SSH alias generation
- Atomic SSH config and known-hosts replacement
- Captured SSH host-key verification
- Read-only action polling
- Snapshot identity lookup and validation
- Mutating `doctl` calls with hidden retries disabled
- Lifecycle-tag reconciliation shared by create, resume, and destroy
- Shared post-boot worker recovery

Preserve Bash 3.2 compatibility. Do not use associative arrays, namerefs,
`mapfile`, or other newer Bash constructs.

### `vps_create.sh`

Before allocating:

- Refuse a paused instance and direct the operator to `vps_resume.sh`.
- Refuse unresolved pause or resume transitions.
- Refuse active snapshot-cleanup-pending state until resume cleanup completes.
- Continue supporting legacy active state.
- Use shared post-boot recovery functions.

Without this guard, creating a known paused instance could incorrectly allocate
from the base Ubuntu image rather than its snapshot.

### `vps_list.sh`

Add a `SNAPSHOT_ID` column and derive status from the transition journal before
terminal active/paused state.

Statuses include:

- `active`
- `paused`
- `pausing-quiescing`
- `pausing-shutdown`
- `pausing-snapshot`
- `pausing-delete-pending`
- `resuming-allocation`
- `resuming-recovery`
- `active-snapshot-cleanup-pending`
- Existing allocation, source-reserved, and orphaned statuses

Listing remains local and read-only. It should not query DigitalOcean for every
row. Malformed state should display an explicit invalid/orphaned status rather
than being reported as active.

### `vps_shell.sh`

- Active: connect normally.
- Paused: refuse with a direct `vps_resume.sh --instance ID` instruction.
- Pausing: refuse and report the current transition phase.
- Resuming before active commit: refuse and report the phase.
- Active with snapshot cleanup pending: permit SSH.

Do not hold the lifecycle lock for the duration of an interactive SSH session.

### `vps_destroy.sh`

Extend teardown to delete the complete resource inventory:

- Active: delete the active Droplet.
- Paused: delete the exact snapshot.
- Pausing: reconcile and delete the source Droplet and any created snapshot.
- Resuming: reconcile and delete any target Droplet and retained snapshot.
- Active cleanup pending: delete the active Droplet and retained snapshot.
- Unresolved target allocation: reconcile through the target lifecycle tag.

Do not remove local state until every potentially billable Droplet and owned
snapshot is confirmed gone or explicitly operator-confirmed.

Credential rules:

- Never revoke the GitHub credential while any Droplet may still be running.
- Once all possible Droplets are confirmed absent, revoke the credential even
  if snapshot deletion failed; revocation protects a stranded snapshot.
- Retain snapshot state until deletion can be retried.

A paused destroy should report that compute billing had already stopped and
that snapshot storage has now been removed. It must not claim to have destroyed
an active Droplet.

### `cloud-init.yaml`

Do not run cloud-init again on snapshot resume. The existing worker bootstrap is
already present in the image.

No cloud-init changes are required for correctness. Explicit remote container
quiescence remains the compatibility path for existing and new workers.

### `README.md` and `config.example.env`

Document:

- Pause, resume, list, shell, and destroy syntax
- Cold-boot rather than RAM-suspend behavior
- Changed Droplet ID and public IP
- `vps_shell.sh` endpoint abstraction
- Snapshot storage billing while paused
- Powered-off Droplet billing before deletion
- Credential-bearing snapshots
- PAT replacement after a long pause
- Supabase/Postgres graceful shutdown and container recovery
- Unsupported attached block-storage volumes
- Avoiding concurrent interactive writes during pause
- All interruption and recovery paths
- Snapshot-cleanup-pending behavior

Initially keep timeout values as internal conservative constants rather than
adding speculative configuration. Add configurable positive-integer timeouts
only if operational use demonstrates a need.

## GitHub and ChatGPT credentials while paused

Do not revoke the GitHub PAT during normal pause. It remains in both the
snapshot and the protected local host token file. It may expire while paused,
which the existing replacement workflow can handle during resume.

The snapshot also contains the ChatGPT/Codex login. Treat the snapshot as a
credential-bearing machine image. It must be deleted during normal resume or
destroy, and it must never be shared or made public.

## DigitalOcean token scopes

In addition to the existing scopes, pause/resume requires:

- `droplet:update` — shutdown, power-off, and Droplet actions
- `image:create` — create a Droplet snapshot
- `snapshot:read` — discover and verify snapshots
- `image:read` — create and verify a Droplet from the private snapshot
- `snapshot:delete` — delete snapshots
- `image:delete` — required dependency of `snapshot:delete`
- `actions:read` — poll asynchronous actions

DigitalOcean documents the dependencies for
[`snapshot:delete`](https://docs.digitalocean.com/reference/api/scopes/snapshot/delete/),
including `image:delete`, `image:read`, `droplet:read`, `regions:read`,
`sizes:read`, `actions:read`, and `snapshot:read`.

Update the README minimum-scope section as part of implementation.

## Recovery flags

Prefer automatic reconciliation. Add only narrowly scoped escape hatches:

- `--confirm-missing-droplet`
- `--confirm-missing-snapshot`
- `--confirm-request-not-accepted`

`--confirm-request-not-accepted` applies only when a journal says a snapshot or
create request may have been submitted, but repeated reconciliation finds no
matching action or resource and the operator independently confirms it was not
accepted.

Do not add flags to:

- Snapshot a running or unquiesced Postgres database
- Ignore an SSH host-key mismatch
- Delete the recovery snapshot before post-boot verification
- Select one of multiple matching Droplets or snapshots

Those are unsafe states rather than ordinary conservative recovery decisions.

## Implementation sequence

### 1. State primitives

- Atomic paused-state writer
- Atomic transition-journal writer
- Strict loaders and invariants
- Snapshot/action lookup helpers
- Shared no-retry mutation wrapper

### 2. Read-only command integration

- Add paused and transitional output to `vps_list.sh`
- Add paused-state handling to `vps_shell.sh`
- Guard `vps_create.sh` against paused/transitional instances

### 3. Pause implementation

- Remote container and Codex quiescence
- Graceful shutdown and action polling
- Snapshot creation and ID discovery
- Source deletion
- Paused-state commit

### 4. Resume implementation

- Snapshot validation
- Lifecycle-tagged replacement allocation
- New endpoint and SSH identity handling
- Shared post-boot recovery
- Snapshot cleanup

### 5. Destroy integration

- Destroy paused state
- Reconcile and destroy every pause phase
- Reconcile and destroy every resume phase
- Preserve existing GitHub token and 404 safety behavior

### 6. Documentation and scopes

- Update README lifecycle instructions
- Update minimum DigitalOcean token scopes
- Document cost, security, IP changes, cold boot, and storage limitations

### 7. Mocked lifecycle tests

Use mocked `doctl`, `ssh`, and temporary local state. Automated tests must not
create, stop, snapshot, resume, or delete real DigitalOcean resources.

## Essential test matrix

### State and locking

- Legacy active state
- Active, paused, and active-cleanup-pending state
- Malformed or contradictory terminal state
- Every transition phase
- Pause/resume/destroy contention on one instance
- Independent instances proceed concurrently
- Existing stale-lock recovery remains unchanged

### Quiescence and shutdown

- No running containers
- Supabase/Postgres containers running
- Mixed running and stopped containers
- Container stop timeout or forced exit
- SSH loss after writing the remote marker
- Normal graceful shutdown
- Shutdown action error
- Shutdown action complete but Droplet still active
- Hard power-off fallback after verified quiescence
- Ambiguous shutdown response with observable off state

### Snapshot creation

- Normal snapshot action and discovery
- Delayed snapshot visibility
- Malformed action response
- Action error
- Lost snapshot-request response
- Zero, one, and multiple exact-name matches
- Matching name from a different source Droplet
- Wrong region or insufficient minimum disk size
- Local state-write failure before source deletion

### Pause deletion

- Confirmed source deletion
- Network loss after deletion
- 404 with and without explicit confirmation
- Interruption after deletion but before paused commit
- Old SSH endpoint files removed before IP reuse

### Resume allocation

- Normal replacement creation
- Lost create response
- Zero, one, and multiple lifecycle-tag matches
- Successful response with duplicate tag matches
- Wrong image, region, name, or size
- Interruption at every persisted phase

### SSH recovery

- Changed public IP
- Old IP later reused by another host
- Preserved SSH host key
- SSH host-key mismatch
- Atomic known-hosts and config replacement
- `vps_shell.sh` uses the new endpoint

### Worker recovery

- Previously running containers restart
- Previously stopped containers remain stopped
- Container health timeout
- GitHub PAT remains valid
- GitHub PAT expires while paused and is replaced
- Credential replacement interrupted at existing journal checkpoints
- Wrong repository origin or branch
- Missing or foreign ChatGPT marker
- Codex daemon restart or pairing failure

### Snapshot cleanup

- Confirmed snapshot deletion
- Lost delete response followed by verified absence
- Unexplained 404 requiring confirmation
- Failed deletion leaves active cleanup-pending state
- Resume retry performs cleanup only
- Pause refused while old snapshot cleanup remains pending

### Destroy integration

- Destroy active instance
- Destroy paused instance
- Destroy every pause phase
- Destroy every resume phase
- Destroy unresolved replacement allocation
- Snapshot deletion failure with credential revocation
- No local cleanup until all owned resources are accounted for

### Command behavior and compatibility

- Every status is displayed correctly by `vps_list.sh`
- `vps_shell.sh` rejects paused and transitional state
- `vps_shell.sh` works after active recovery despite pending snapshot cleanup
- `bash -n` for every script
- ShellCheck for every script
- Bash 3.2 execution paths
- No associative arrays, namerefs, or post-Bash-3.2 syntax

## Critical invariants

1. A directly verified snapshot ID must be durably recorded before deleting the
   source Droplet.
2. The source Droplet is never considered non-billable until deletion is
   confirmed or explicitly operator-confirmed.
3. A snapshot is retained until the replacement Droplet, workspace,
   credentials, containers, and Codex control plane are fully verified.
4. An uncertain mutation is reconciled before it is retried.
5. Multiple matching provider resources always fail closed.
6. `vps_destroy.sh` accounts for every potentially owned Droplet and snapshot.
7. Credential revocation never occurs while a Droplet may still be running.
8. A paused snapshot is treated as credential-bearing sensitive storage.
