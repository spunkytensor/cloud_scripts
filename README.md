# Disposable cloud Codex worker

`vps` creates disposable cloud development environments with a repository checkout, standalone GitHub access, and persistent headless control from an authorized Codex/ChatGPT client. DigitalOcean is the default backend; backend identity is explicit in state and selectable with `--backend`.

The remote Codex process operates directly on the VPS checkout. The local Rust executable is the control plane. The original Bash commands remain in this migration release so operators can recover pre-Rust state while the native CLI is validated against real provider lifecycles; do not alternate Bash and Rust mutations for one instance.

## Install and build `vps`

Tagged releases publish checksummed archives for macOS arm64/x64 and Windows x64, plus Ubuntu amd64/arm64 `.deb` packages. Release assets include shell completions, a man page, third-party notices, CycloneDX SBOMs, and build provenance.

Builds are pinned to Rust 1.96.1 with edition 2024:

```bash
rustup toolchain install 1.96.1 --component rustfmt,clippy
cargo build --locked --release
./target/release/vps --help
```

The executable uses the DigitalOcean REST API directly; it does not require `doctl`. It currently requires the system OpenSSH client for SSH execution and interactive shells. Set the cloud token only on the control machine:

```bash
export DIGITALOCEAN_TOKEN='...'
```

Create a TOML configuration such as:

```toml
version = 1
default_backend = "digitalocean"

[ssh]
private_key = "~/.ssh/id_digitalocean_v2"

[git]
author_name = "Codex VPS Agent"
author_email = "codex-vps-agent@users.noreply.github.com"

[backends.digitalocean]
ssh_key = "digitalocean-key-id-or-fingerprint"
region = "nyc3"
size = "s-4vcpu-8gb"
image = "ubuntu-24-04-x64"
tags = ["codex-agent"]
name_prefix = "codex-agent"
```

Select it with `--config` or `VPS_CONFIG`. CLI values override environment values, which override TOML and defaults. Tokens are never accepted in TOML.

The single-command lifecycle is:

```bash
vps --config vps.toml create --new example-org/testrepo --branch main
vps list
vps status frontend-a
vps shell frontend-a
vps pause frontend-a
vps resume frontend-a
vps destroy frontend-a
```

Use `--backend digitalocean` to select the backend explicitly; it is the default. `vps doctor` validates configuration, local state, and provider access. `vps completion <shell>` and `vps man` generate packaging resources.

The Rust state defaults to the platform state directory for new installations and detects repository-local `.state/instances` during migration. `vps config migrate config.env` converts non-secret legacy settings without sourcing or executing the file. Lifecycle commands strictly import current active, paused, allocation, and pause/resume transition files under the instance lock, retain the legacy evidence, and continue the same persisted operation rather than allocating a replacement blindly.

Provider mutations are journaled before submission. A lost response is reconciled through the persisted correlation identity; zero matches remains uncertain and multiple matches fail closed. Rerun the same command after interruption. Never delete local state merely to bypass an uncertain provider operation.

## Purpose

This repository exists to move agent-driven development off the developer's laptop and onto disposable machines, one machine per unit of work.

**Keep the agent off the developer machine.** A coding agent that can run arbitrary commands, install dependencies, and execute repository code is a poor fit for a laptop that also holds personal credentials, SSH keys, cloud logins, and unrelated client work. Each worker here is a separate VPS with only what the job needs: a checkout of one repository and a GitHub credential scoped to that same repository. The agent gets outbound network access and root-equivalent Docker access inside that VPS, and none of it touches the laptop. When the work is done, `vps_destroy.sh` deletes the machine and revokes the credential, so a compromised or misbehaving agent loses everything it had rather than persisting on a long-lived host. The laptop keeps only the control plane and the DigitalOcean credential, which is never copied to the VPS.

**Run work in parallel instead of in series.** A single developer machine forces agents to take turns: one checkout, one set of ports, one dependency tree, one running stack. Because each worker here is an independent machine, several agents can work at once — separate features, separate repositories, competing approaches to the same problem, or a long-running migration alongside ordinary development. `vps_create.sh --new OWNER/REPOSITORY,BASE_BRANCH` allocates another one, `vps_list.sh` shows what is running, and each instance keeps its own Droplet, repository source, work branch, credential, and SSH configuration under `.state/instances/<instance-id>/`. Throughput is limited by what you are willing to pay for and supervise, not by the one machine in front of you.

**Give every copy its own operating system.** Parallel work on a shared host means contending for the same ports, the same Docker daemon, the same database sockets, the same global toolchain versions, and the same filesystem paths — and it means one agent's broken state can break another's. Full-machine isolation removes that entire category of problem. Each worker boots its own Ubuntu instance, so every copy of the stack can bind the same conventional ports, run its own Supabase containers and databases, and install whatever system packages it needs without coordination. It also makes the environment genuinely reproducible: cloud-init builds each machine identically from scratch, so a worker reflects the repository's real requirements rather than a laptop's accumulated local configuration. Every new VPS is a fresh, clean environment with no leftover dependencies, caches, configuration, or processes from earlier work, and any containers it launches start from that clean foundation. Damage is contained to one disposable machine, and recovering means destroying it and creating another.

## Security and cost model

- A Droplet is billable until `vps_destroy.sh` successfully deletes it. Powering it off is not enough.
- `vps_pause.sh` performs a cold suspend: it shuts down Docker workloads, snapshots the boot disk, and deletes the Droplet. Compute billing stops only after deletion is confirmed; the private snapshot remains billed as storage (currently $0.06/GB/month, subject to DigitalOcean pricing changes).
- A paused snapshot contains the checkout, databases, GitHub PAT, and ChatGPT login. Treat it as a credential-bearing private machine image; never share or publish it. Normal resume and destroy delete it.
- Remote control performs a separate ChatGPT device login on the VPS and never copies the local Codex credential.
- A persistent remote-control workspace uses a dedicated fine-grained PAT restricted to the repository selected for that instance. The VPS keeps that credential so its agent can run both Git and `gh` operations without the host. The provisioning host retains a mode-`0600` copy only so teardown can revoke it if the VPS is unreachable.
- The Codex process necessarily has access to the VPS's ChatGPT and GitHub credentials. Repository code running as `agent` could attempt to read or exfiltrate them, so use this workflow only with repositories you trust.
- The remote agent receives outbound network access and root-equivalent Docker socket access so it can run local development infrastructure such as Supabase. Repository code can therefore control the VPS through Docker; use only trusted repositories and dependencies. The agent does not receive DigitalOcean credentials.
- Cloud-init authorizes the configured shared SSH key for `agent` and disables root SSH login. All post-provisioning SSH operations run directly as the unprivileged `agent` user.
- SSH bootstrap uses trust on first use (`StrictHostKeyChecking=accept-new`) with a dedicated known-hosts file. This is convenient for an ephemeral worker but does not protect the first connection from an active network attacker. Use an SSH host CA or another independently verified host key before using this pattern in a hostile network.

## Prerequisites

The following subsection documents the retained Bash migration commands. For the Rust CLI, install OpenSSH and provide `DIGITALOCEAN_TOKEN`; `doctl`, local `gh`, and local `jq` are not runtime requirements.

Install and authenticate the local control-plane tools for your operating system:

<details>
<summary>macOS (Homebrew)</summary>

```bash
brew install doctl gh jq
doctl auth init
gh auth login
```

</details>

<details>
<summary>Ubuntu 24.04 or 26.04</summary>

```bash
sudo apt-get update
sudo apt-get install -y gh git jq openssh-client snapd
sudo snap install doctl
doctl auth init
gh auth login
```

</details>

### Minimum DigitalOcean token scopes

Create a custom-scoped DigitalOcean personal access token with these scopes for the scripts as configured. See DigitalOcean's [custom scope reference](https://docs.digitalocean.com/reference/api/scopes/) for the current definitions and dependencies.

- `droplet:create`, `droplet:read`, `droplet:update`, and `droplet:delete` — create, poll, shut down, and destroy the worker.
- `regions:read`, `sizes:read`, `actions:read`, and `image:read` — required dependencies of the Droplet create/delete scopes.
- `image:create`, `snapshot:read`, `snapshot:delete`, and `image:delete` — create, verify, and remove private recovery snapshots. `image:delete`, `image:read`, `droplet:read`, `regions:read`, `sizes:read`, `actions:read`, and `snapshot:read` are dependencies of `snapshot:delete`.
- `vpc:read` — an additional dependency enforced by DigitalOcean's token-creation UI.
- `ssh_key:read` — embed the existing `DO_SSH_KEY` in the new Droplet. It also permits the `doctl compute ssh-key list` discovery command below.
- `tag:create` and `tag:read` — apply `DO_TAGS` during creation; `tag:read` is required by `tag:create`.

`--enable-monitoring` installs the monitoring agent on the Droplet and does **not** require `monitoring:create`; that scope creates Monitoring alert policies, which these scripts do not manage. The scripts do not require any broad full-access scope.

Find the DigitalOcean SSH key ID or fingerprint to place in the configuration:

```bash
doctl compute ssh-key list
```

If the corresponding private key is encrypted, load it before allocating so the non-interactive SSH checks can use it:

```bash
ssh-add --apple-use-keychain ~/.ssh/id_digitalocean_v2
```

## Configure

```bash
cd vps-codex
cp config.example.env config.env
$EDITOR config.env
```

At minimum, set:

- `DO_SSH_KEY`
- `SSH_PRIVATE_KEY_FILE` to the corresponding private key on the local control machine

GitHub requires a one-time interactive confirmation to create each instance's fine-grained PAT; neither the REST API nor `gh` can create it unattended. `vps_create.sh` prints a pre-filled creation URL and securely prompts for the result. Tokens cannot be supplied through `config.env`, ensuring teardown can revoke one instance without affecting another.

## Run the lifecycle

### Create or resume the environment

```bash
./vps_create.sh --new example-org/testrepo,main
```

The script:

1. Allocates the Droplet, or resumes the Droplet recorded in `.state/`.
2. Waits for its public IP, SSH, and cloud-init.
3. Creates or verifies the VPS-specific GitHub credential.
4. Clones the requested repository into `/home/agent/projects` and ensures branch `codex/<instance-id>` exists.
5. Starts a TTY-backed ChatGPT device login when needed.
6. Starts Codex remote control and prints a fresh pairing code.

If setup is interrupted after allocation, the Droplet remains allocated and may be billable. Resume with the complete instance command printed by the script, such as `./vps_create.sh --instance worker-20260726-120000-12345-6789 example-org/testrepo,main`; it verifies and resumes the existing token, checkout, login, and daemon instead of allocating another machine. The repository and base branch must match the source persisted for that instance; a mismatch is refused before credential or checkout mutation. If the retained two-day token expired, `vps_create.sh` asks for and validates a replacement while preserving the checkout and branch. A protected replacement journal records both credential values until the old one is revoked. Use `./vps_destroy.sh --instance <instance-id>` to abandon the setup.

The GitHub prompt creates a fine-grained PAT. Verify that it:

- Targets only the repository passed to `vps_create.sh`.
- Expires in two days.
- Grants `Contents: read and write`, `Pull requests: read and write`, `Actions: read`, and `Commit statuses: read`.
- Has any organization-required approval.

The VPS then stands on its own. Both interactive SSH shells and the Codex-controlled environment can use ordinary commands such as:

```bash
cd /home/agent/projects/your-repository
git fetch
git push -u origin HEAD
gh pr create
gh run list
gh run watch
```

The `agent` user can also run Docker directly, including the containers started by the Supabase CLI. This is intentionally root-equivalent access inside the disposable VPS; no `sudo`, password, `newgrp`, or manual socket permission change is needed on a newly provisioned environment.

Repository code running as `agent` can necessarily read this credential. Keep default-branch protection enabled and do not grant the token ruleset-bypass or repository-administration permissions. Fine-grained PATs do not support every GitHub API, including some Checks API operations; use `gh run` for Actions monitoring and test any additional required `gh` commands before relying on them unattended.

Remote control requires Codex CLI 0.143.0 or newer. The VPS login must use the same ChatGPT account and workspace as the controlling client. API-key and access-token logins are rejected because they do not enroll the host for direct remote control. No inbound app-server port is opened; the daemon uses the Codex secure relay.

The daemon survives SSH logout but not a VPS reboot. Rerun `./vps_create.sh --instance <instance-id> OWNER/REPOSITORY,BASE_BRANCH` after reboot to restart it and obtain a new pairing code.

Open an interactive shell using the instance ID or name:

```bash
./vps_shell.sh worker-20260726-120000-12345-6789
```

`vps_shell.sh` reads the concrete OpenSSH config path and host alias generated
for that exact instance, then runs the equivalent of
`ssh -F "$SSH_CONFIG_FILE" "$SSH_ALIAS"`.

To let the Codex desktop app discover the host, add this line to `~/.ssh/config` using the absolute generated path:

```sshconfig
Include /absolute/path/to/vps-codex/.state/instances/*/current.env.ssh_config
```

Remote root login is disabled.

Ordinary `codex` or `codex exec` sessions started separately over SSH cannot be attached to as live remote-control sessions. Start new work through the paired client.

### Pause and resume the environment

```bash
./vps_pause.sh --instance frontend-a
./vps_list.sh
./vps_resume.sh --instance frontend-a
./vps_shell.sh frontend-a
```

Pause is a cold boot cycle, not a RAM suspend. Running processes are lost. Before shutdown, the script verifies the worker was provisioned with snapshot-safe cloud-init host-key preservation, stops Codex remote control, records the running and healthy Docker containers, gracefully stops them (including Supabase/Postgres), runs `sync`, and verifies the SSH host key. It then powers off the Droplet, creates and verifies a private boot-disk snapshot, and deletes the Droplet. Workers created before this support was added must be recreated before they can be paused. Do not pause while another operator or process is independently writing through SSH; the local lifecycle lock cannot prevent an unrelated shell from changing the disk.

Attached DigitalOcean block-storage volumes are not captured by a Droplet snapshot and are rejected by this initial pause implementation. Docker named volumes and database data on the Droplet's boot disk are preserved.

Resume creates a replacement Droplet from the recorded snapshot in the original region and size. Its Droplet ID and public IP change, but the generated alias remains `codex-vps-<instance-id>` and `vps_shell.sh` always reads the latest endpoint. Resume requires the preserved SSH Ed25519 host key, restarts only the containers that were running before pause, waits for previously healthy containers, verifies the checkout and credentials, restarts remote control, and prints a new pairing code. The snapshot is deleted only after those checks pass.

If the repository-scoped two-day PAT expires during a long pause, resume prompts for and validates a replacement fine-grained PAT while retaining a protected replacement journal until the old credential is revoked. The recovery snapshot is not deleted until this succeeds. The ChatGPT login is expected to survive in the snapshot; a missing or foreign ownership marker is refused.

Every pause/resume provider mutation is journaled beneath `.state/instances/<instance>/`. Rerun the same command after interruption. `vps_list.sh` reports phases such as `pausing-snapshot`, `resuming-recovery`, and `active-snapshot-cleanup-pending`. Shell access is refused during unsafe phases, but remains available when only snapshot cleanup is pending. In that final state, rerun `vps_resume.sh`; do not start another pause until cleanup succeeds.

Provider 404s are conservative because they can indicate the wrong `doctl` account. After independently verifying the account and absence of the exact resource, use `--confirm-missing-droplet` or `--confirm-missing-snapshot` as directed. Use `--confirm-request-not-accepted` only when a journaled snapshot/create request has no matching provider resource and you have independently confirmed DigitalOcean never accepted it.

### Destroy the environment

```bash
./vps_destroy.sh --instance worker-20260726-120000-12345-6789
```

Run this when development is complete, including after a setup, pause, or resume failure. It inventories the active/source/replacement Droplets and retained snapshot from local journals, confirms all possible Droplets are absent before revoking the PAT, and removes the credential-bearing snapshot. If snapshot deletion fails after Droplet deletion, credential revocation still proceeds and snapshot ownership state remains for retry. After independently revoking the token or accepting its remaining expiration window, add `--forget-unrevoked-token`. Remove the environment from the controlling client if it remains listed after destruction.

A DigitalOcean 404 may mean either that a resource was deleted elsewhere or that `doctl` is authenticated to a different account. Teardown retains credentials and state until you verify the account and rerun with the applicable `--confirm-missing-droplet` or `--confirm-missing-snapshot` flag.

If allocation was interrupted before DigitalOcean returned an ID, both scripts reconcile it through a unique lifecycle tag. A zero-match lookup is treated as temporarily uncertain and retains its checkpoint. Only after independently confirming that no tagged Droplet exists should you add `--forget-unresolved-allocation` to that instance's destroy command.

## Multiple concurrent workers

One configuration is a reusable infrastructure profile. Create as many independent instances of that profile as needed, including instances based on different repositories or base branches:

```bash
./vps_create.sh --new example-org/frontend,main
./vps_create.sh --new example-org/backend,develop
```

Each invocation prints its generated instance ID before allocating and again in the final connection summary. Instances receive separate Droplets, repository sources, generated work branches, credentials, SSH files, and lifecycle locks beneath `.state/instances/<instance-id>/`. `vps_list.sh` includes each instance's persisted repository and base branch.

List locally known instances at any time:

```bash
./vps_list.sh
```

Open a shell on one instance:

```bash
./vps_shell.sh frontend-a
```

Resume one instance after interruption or reboot:

```bash
./vps_create.sh --instance worker-20260726-120000-12345-6789 example-org/frontend,main
```

Cold-pause and resume one instance while preserving its boot disk:

```bash
./vps_pause.sh --instance frontend-a
./vps_resume.sh --instance frontend-a
```

Destroy that exact instance:

```bash
./vps_destroy.sh --instance worker-20260726-120000-12345-6789
```

You may also choose a memorable ID instead of generating one; the same command creates it when absent and resumes it when present:

```bash
./vps_create.sh --instance frontend-a example-org/frontend,main
./vps_create.sh --instance backend-a example-org/backend,develop
```

`vps_create.sh` always requires `--new` or `--instance` plus exactly one repository/base-branch argument. `vps_destroy.sh` always requires only `--instance`, because the instance state records the exact Droplet and credential to remove. There is no unscoped single-instance state or bare destroy operation.
