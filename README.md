# Disposable DigitalOcean Codex worker

These scripts create a disposable DigitalOcean development environment with a repository checkout, standalone GitHub access, and persistent headless control from an authorized Codex/ChatGPT client.

The remote Codex process operates directly on the VPS checkout. The local scripts are the control plane.

## Purpose

This repository exists to move agent-driven development off the developer's laptop and onto disposable machines, one machine per unit of work.

**Keep the agent off the developer machine.** A coding agent that can run arbitrary commands, install dependencies, and execute repository code is a poor fit for a laptop that also holds personal credentials, SSH keys, cloud logins, and unrelated client work. Each worker here is a separate VPS with only what the job needs: a checkout of one repository and a GitHub credential scoped to that same repository. The agent gets outbound network access and root-equivalent Docker access inside that VPS, and none of it touches the laptop. When the work is done, `vps_destroy.sh` deletes the machine and revokes the credential, so a compromised or misbehaving agent loses everything it had rather than persisting on a long-lived host. The laptop keeps only the control plane and the DigitalOcean credential, which is never copied to the VPS.

**Run work in parallel instead of in series.** A single developer machine forces agents to take turns: one checkout, one set of ports, one dependency tree, one running stack. Because each worker here is an independent machine, several agents can work at once — separate features, separate repositories, competing approaches to the same problem, or a long-running migration alongside ordinary development. `vps_create.sh --new OWNER/REPOSITORY,BASE_BRANCH` allocates another one, `vps_list.sh` shows what is running, and each instance keeps its own Droplet, repository source, work branch, credential, and SSH configuration under `.state/instances/<instance-id>/`. Throughput is limited by what you are willing to pay for and supervise, not by the one machine in front of you.

**Give every copy its own operating system.** Parallel work on a shared host means contending for the same ports, the same Docker daemon, the same database sockets, the same global toolchain versions, and the same filesystem paths — and it means one agent's broken state can break another's. Full-machine isolation removes that entire category of problem. Each worker boots its own Ubuntu instance, so every copy of the stack can bind the same conventional ports, run its own Supabase containers and databases, and install whatever system packages it needs without coordination. It also makes the environment genuinely reproducible: cloud-init builds each machine identically from scratch, so a worker reflects the repository's real requirements rather than a laptop's accumulated local configuration. Every new VPS is a fresh, clean environment with no leftover dependencies, caches, configuration, or processes from earlier work, and any containers it launches start from that clean foundation. Damage is contained to one disposable machine, and recovering means destroying it and creating another.

## Security and cost model

- A Droplet is billable until `vps_destroy.sh` successfully deletes it. Powering it off is not enough.
- Remote control performs a separate ChatGPT device login on the VPS and never copies the local Codex credential.
- A persistent remote-control workspace uses a dedicated fine-grained PAT restricted to the repository selected for that instance. The VPS keeps that credential so its agent can run both Git and `gh` operations without the host. The provisioning host retains a mode-`0600` copy only so teardown can revoke it if the VPS is unreachable.
- The Codex process necessarily has access to the VPS's ChatGPT and GitHub credentials. Repository code running as `agent` could attempt to read or exfiltrate them, so use this workflow only with repositories you trust.
- The remote agent receives outbound network access and root-equivalent Docker socket access so it can run local development infrastructure such as Supabase. Repository code can therefore control the VPS through Docker; use only trusted repositories and dependencies. The agent does not receive DigitalOcean credentials.
- Cloud-init authorizes the configured shared SSH key for `agent` and disables root SSH login. All post-provisioning SSH operations run directly as the unprivileged `agent` user.
- SSH bootstrap uses trust on first use (`StrictHostKeyChecking=accept-new`) with a dedicated known-hosts file. This is convenient for an ephemeral worker but does not protect the first connection from an active network attacker. Use an SSH host CA or another independently verified host key before using this pattern in a hostile network.

## Prerequisites

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

- `droplet:create`, `droplet:read`, and `droplet:delete` — create, poll, and destroy the worker.
- `regions:read`, `sizes:read`, `actions:read`, and `image:read` — required dependencies of the Droplet create/delete scopes.
- `snapshot:read` and `vpc:read` — additional dependencies enforced by DigitalOcean's token-creation UI for these selections.
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

### Destroy the environment

```bash
./vps_destroy.sh --instance worker-20260726-120000-12345-6789
```

Run this when development is complete, including after a setup failure. It attempts to stop the remote-control daemon, deletes the exact Droplet ID recorded for that instance, and then revokes its retained fine-grained PAT. If an unknown deletion failure occurs, it leaves the PAT and state active for a safe retry. If deletion succeeds but revocation fails, rerun the same instance-specific destroy command. After independently revoking the token or accepting its remaining expiration window, add `--forget-unrevoked-token`. Remove the environment from the controlling client if it remains listed after destruction.

A DigitalOcean 404 may mean either that the Droplet was deleted elsewhere or that `doctl` is authenticated to a different account. Teardown revokes the PAT on 404 but retains the Droplet state. After verifying the account and confirming the Droplet is absent, rerun the instance-specific command with `--confirm-missing-droplet`.

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
