# Disposable DigitalOcean Codex worker

These scripts create a disposable DigitalOcean development environment with a repository checkout, standalone GitHub access, and persistent headless control from an authorized Codex/ChatGPT client.

The remote Codex process operates directly on the VPS checkout. The local scripts are the control plane.

## Security and cost model

- A Droplet is billable until `destroy.sh` successfully deletes it. Powering it off is not enough.
- Remote control performs a separate ChatGPT device login on the VPS and never copies the local Codex credential.
- A persistent remote-control workspace uses a dedicated fine-grained PAT restricted to the configured repository. The VPS keeps that credential so its agent can run both Git and `gh` operations without the host. The provisioning host retains a mode-`0600` copy only so teardown can revoke it if the VPS is unreachable.
- The Codex process necessarily has access to the VPS's ChatGPT and GitHub credentials. Repository code running as `agent` could attempt to read or exfiltrate them, so use this workflow only with repositories you trust.
- The remote agent receives outbound network access and root-equivalent Docker socket access so it can run local development infrastructure such as Supabase. Repository code can therefore control the VPS through Docker; use only trusted repositories and dependencies. The agent does not receive DigitalOcean credentials.
- Cloud-init authorizes the configured shared SSH key for `agent` and disables root SSH login. All post-provisioning SSH operations run directly as the unprivileged `agent` user.
- Never provision the token returned by the host's `gh auth token` as the persistent VPS credential. Create a dedicated fine-grained PAT restricted to one repository, with a two-day expiration and only the required permissions.
- SSH bootstrap uses trust on first use (`StrictHostKeyChecking=accept-new`) with a dedicated known-hosts file. This is convenient for an ephemeral worker but does not protect the first connection from an active network attacker. Use an SSH host CA or another independently verified host key before using this pattern in a hostile network.

## Prerequisites

Install and authenticate the local tools:

```bash
brew install doctl gh jq
doctl auth init
gh auth login
```

### Minimum DigitalOcean token scopes

Create a custom-scoped DigitalOcean personal access token with these scopes for the scripts as configured:

- `droplet:create`, `droplet:read`, and `droplet:delete` — create, poll, and destroy the worker.
- `regions:read`, `sizes:read`, `actions:read`, and `image:read` — required dependencies of the Droplet create/delete scopes.
- `snapshot:read` and `vpc:read` — additional dependencies enforced by DigitalOcean's token-creation UI for these selections.
- `ssh_key:read` — embed the existing `DO_SSH_KEY` in the new Droplet. It also permits the `doctl compute ssh-key list` discovery command below.
- `tag:create` and `tag:read` — apply `DO_TAGS` during creation; `tag:read` is required by `tag:create`.

`--enable-monitoring` installs the monitoring agent on the Droplet and does **not** require `monitoring:create`; that scope creates Monitoring alert policies, which these scripts do not manage. The scripts do not require any broad full-access scope.

Use a dedicated, narrowly scoped token for this workflow. Keep it only in the local `doctl` authentication context: do not copy it to the VPS, put it in `config.env`, or commit it. See DigitalOcean's [custom scope reference](https://docs.digitalocean.com/reference/api/scopes/) for the current scope definitions and dependencies.

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
- `REPOSITORY`
- `BASE_BRANCH`

`REPOSITORY` can be `owner/repository` or a GitHub URL.
Leave `WORK_BRANCH` blank to generate a unique branch when the environment is first created; that concrete branch is persisted across every resume. Alternatively, set a fixed branch explicitly. Changing an explicit `WORK_BRANCH` while its environment exists fails loudly instead of continuing work on the previously recorded branch.

GitHub requires a one-time interactive confirmation to create the fine-grained PAT; neither the REST API nor `gh` can create it unattended. `create.sh` prints a pre-filled creation URL and securely prompts for the result. As an alternative, set `GITHUB_TOKEN_FILE` to a protected local file containing the dedicated token. Do not put the token itself in `config.env`.

## Run the lifecycle

### Create or resume the environment

```bash
./create.sh
```

The script:

1. Allocates the Droplet, or resumes the Droplet recorded in `.state/`.
2. Waits for its public IP, SSH, and cloud-init.
3. Creates or verifies the VPS-specific GitHub credential.
4. Clones `REPOSITORY` into `/home/agent/projects` and ensures `WORK_BRANCH` exists.
5. Starts a TTY-backed ChatGPT device login when needed.
6. Starts Codex remote control and prints a fresh pairing code.

If setup is interrupted after allocation, the Droplet remains allocated and may be billable. Rerun `./create.sh`; it verifies and resumes the existing token, checkout, login, and daemon instead of allocating another machine. If the retained two-day token expired, `create.sh` asks for and validates a replacement, confirms the old remote copy before replacing it, and preserves the checkout and work branch. A protected replacement journal records both credential values until the old one is revoked, so either resume or teardown can safely reconcile an interruption. Use `./destroy.sh` to abandon the setup.

The GitHub prompt creates a fine-grained PAT. Verify that it:

- Targets only `REPOSITORY`.
- Expires in two days.
- Grants `Contents: read and write`, `Pull requests: read and write`, `Actions: read`, and `Commit statuses: read`.
- Has any organization-required approval.

The VPS then stands on its own. Both interactive SSH shells and the Codex-controlled environment can use ordinary commands such as:

```bash
cd /home/agent/projects/your-repository
git fetch
git push -u origin "$WORK_BRANCH"
gh pr create
gh run list
gh run watch
```

The `agent` user can also run Docker directly, including the containers started by the Supabase CLI. This is intentionally root-equivalent access inside the disposable VPS; no `sudo`, password, `newgrp`, or manual socket permission change is needed on a newly provisioned environment.

Repository code running as `agent` can necessarily read this credential. Keep default-branch protection enabled and do not grant the token ruleset-bypass or repository-administration permissions. Fine-grained PATs do not support every GitHub API, including some Checks API operations; use `gh run` for Actions monitoring and test any additional required `gh` commands before relying on them unattended.

Remote control requires Codex CLI 0.143.0 or newer. The VPS login must use the same ChatGPT account and workspace as the controlling client. API-key and access-token logins are rejected because they do not enroll the host for direct remote control. No inbound app-server port is opened; the daemon uses the Codex secure relay.

The daemon survives SSH logout but not a VPS reboot. Rerun `./create.sh` after reboot to restart it and obtain a new pairing code.

`create.sh` writes a concrete OpenSSH host entry to `.state/current.env.ssh_config`. Use it directly:

```bash
source .state/current.env
ssh -F "$SSH_CONFIG_FILE" "$SSH_ALIAS"
```

To let the Codex desktop app discover the host, add this line to `~/.ssh/config` using the absolute generated path:

```sshconfig
Include /absolute/path/to/vps-codex/.state/current.env.ssh_config
```

Remote root login is disabled.

Ordinary `codex` or `codex exec` sessions started separately over SSH cannot be attached to as live remote-control sessions. Start new work through the paired client.

### Destroy the environment

```bash
./destroy.sh
```

Run this when development is complete, including after a setup failure. It attempts to stop the remote-control daemon, deletes the Droplet rather than merely powering it off, and then revokes the retained fine-grained PAT through GitHub's credential revocation API. Revocation uses the host copy and therefore works even if SSH is unavailable. If an unknown deletion failure occurs, it leaves the PAT active and retains both Droplet and token state for a safe retry. If deletion succeeds but GitHub revocation fails, the protected token is retained and rerunning `destroy.sh` retries only revocation. After independently revoking the token or explicitly accepting its remaining expiration window, escape an unrecoverable revocation failure with `./destroy.sh --forget-unrevoked-token`. Remove the environment from the controlling client if it remains listed after destruction.

A DigitalOcean 404 may mean either that the Droplet was deleted elsewhere or that `doctl` is authenticated to a different account. To avoid leaving repository authority live after out-of-band deletion, teardown revokes the PAT on 404 but retains the Droplet state. After verifying the account and confirming the Droplet is absent, run `./destroy.sh --confirm-missing-droplet` to remove the retained local state.

If allocation was interrupted before DigitalOcean returned an ID, both scripts reconcile it through a unique lifecycle tag. A zero-match lookup is treated as temporarily uncertain and retains its checkpoint. Only after independently confirming that no tagged Droplet exists should you clear it with `./destroy.sh --forget-unresolved-allocation`.

## Multiple concurrent workers

Give each process a distinct configuration and state file, and ensure every worker has a unique Droplet and Git branch:

```bash
VPS_CODEX_CONFIG="$PWD/config-api.env" ./create.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./destroy.sh
```

Each config should set a distinct `VPS_STATE_FILE`, `DROPLET_NAME`, and `WORK_BRANCH`.

## Operational hardening

For repeated or unattended use, add an external TTL reaper in a separate trusted environment. It should enumerate resources tagged `codex-agent` and delete expired or orphaned Droplets. Local shell traps cannot clean up after laptop failure, network loss, or a killed process.

Also consider replacing first-use SSH host-key acceptance with an SSH host CA. If this workflow grows beyond a small number of short-lived workers, replace manually created fine-grained PATs with a GitHub App and a trusted token broker; GitHub App installation tokens expire after one hour and cannot be issued with a two-day lifetime.
