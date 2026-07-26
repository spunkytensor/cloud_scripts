# Disposable DigitalOcean Codex worker

These scripts create a disposable DigitalOcean development environment with a repository checkout, standalone GitHub access, and persistent headless control from an authorized Codex/ChatGPT client.

The remote Codex process operates directly on the VPS checkout. The local scripts are the control plane.

## Security and cost model

- A Droplet is billable until `03-destroy.sh` successfully deletes it. Powering it off is not enough.
- Remote control performs a separate ChatGPT device login on the VPS and never copies the local Codex credential.
- A persistent remote-control workspace uses a dedicated fine-grained PAT restricted to the configured repository. The VPS keeps that credential so its agent can run both Git and `gh` operations without the host. The provisioning host retains a mode-`0600` copy only so teardown can revoke it if the VPS is unreachable.
- The Codex process necessarily has access to the VPS's ChatGPT and GitHub credentials. Repository code running as `agent` could attempt to read or exfiltrate them, so use this workflow only with repositories you trust.
- The remote agent receives outbound network access. Docker is installed, but the agent is deliberately not in the root-equivalent `docker` group. It does not receive DigitalOcean credentials.
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
- `WORK_BRANCH`

`REPOSITORY` can be `owner/repository` or a GitHub URL.

For persistent remote-control work, GitHub requires a one-time interactive confirmation to create the fine-grained PAT; neither the REST API nor `gh` can create it unattended. `02-provision-github-workspace.sh` prints a pre-filled creation URL and securely prompts for the result. As an alternative, set `GITHUB_TOKEN_FILE` to a protected local file containing the dedicated token. Do not put the token itself in `config.env`.

## Run the lifecycle

### 1. Allocate and bootstrap the VPS

```bash
./01-allocate.sh
```

The script records the Droplet ID, IP, and a dedicated SSH known-hosts file under `.state/`. If provisioning fails after DigitalOcean creates the machine, this state remains available for cleanup.

It also writes a concrete OpenSSH host entry to `.state/current.env.ssh_config`. Use it directly:

```bash
source .state/current.env
ssh -F "$SSH_CONFIG_FILE" "$SSH_ALIAS"
```

To let the Codex desktop app discover the host, add this line to `~/.ssh/config` using the absolute generated path:

```sshconfig
Include /absolute/path/to/vps-codex/.state/current.env.ssh_config
```

Remote root login is disabled.

### 2A. Provision a standalone GitHub workspace

```bash
./02-provision-github-workspace.sh
```

The script prints a pre-filled GitHub fine-grained PAT creation URL. In GitHub, verify that the token:

- Targets only `REPOSITORY`.
- Expires in two days.
- Grants `Contents: read and write`, `Pull requests: read and write`, `Actions: read`, and `Commit statuses: read`.
- Has any organization-required approval.

After you paste the token into the hidden prompt, the script validates repository access, stores protected copies on the host and VPS, configures HTTPS Git authentication, clones into `/home/agent/projects`, configures repository-local commit identity, and creates `WORK_BRANCH`. The token never appears in the Git remote URL or `config.env`.

The VPS then stands on its own. Both interactive SSH shells and the Codex-controlled environment can use ordinary commands such as:

```bash
cd /home/agent/projects/your-repository
git fetch
git push -u origin "$WORK_BRANCH"
gh pr create
gh run list
gh run watch
```

Repository code running as `agent` can necessarily read this credential. Keep default-branch protection enabled and do not grant the token ruleset-bypass or repository-administration permissions. Fine-grained PATs do not support every GitHub API, including some Checks API operations; use `gh run` for Actions monitoring and test any additional required `gh` commands before relying on them unattended.

### 2B. Enable persistent headless remote control

Remote control requires Codex CLI 0.143.0 or newer. Allocation verifies that minimum version. Then run:

```bash
./02-enable-remote-control.sh
```

The script:

1. Verifies the remote Codex version.
2. Starts a headless ChatGPT device-login flow if the VPS is not logged in. Open the displayed URL on an authorized device and enter its code.
3. Starts the detached Codex remote-control daemon.
4. Prints a short-lived manual pairing code for the Codex/ChatGPT client.

The VPS login must use the same ChatGPT account and workspace as the controlling client. API-key and access-token logins are rejected because they do not enroll the host for direct remote control. No inbound app-server port is opened; the daemon uses the Codex secure relay.

The daemon survives SSH logout, but not a VPS reboot. Rerun `02-enable-remote-control.sh` after reboot to restart it and obtain a new pairing code.

Workspace provisioning and remote-control enrollment are intentionally compatible: the agent needs both the VPS-specific GitHub credential and its ChatGPT login. Run workspace provisioning before pairing if you want the agent to begin with the checkout, or run it afterward without reenrolling remote control.

Ordinary `codex` or `codex exec` sessions started separately over SSH cannot be attached to as live remote-control sessions. Start new work through the paired client.

### 3. Stop remote control and destroy the VPS

```bash
./03-destroy.sh
```

Run this when development is complete, including after a provisioning or remote-control failure. It attempts to stop the remote-control daemon, deletes the Droplet rather than merely powering it off, and then revokes the retained fine-grained PAT through GitHub's credential revocation API. Revocation uses the host copy and therefore works even if SSH is unavailable. If Droplet deletion is not confirmed, the script leaves the PAT active and retains both Droplet and token state for a safe retry. If deletion succeeds but GitHub revocation fails, the protected token is retained and rerunning `03-destroy.sh` retries only revocation. Remove the environment from the controlling client if it remains listed after destruction.

## Multiple concurrent workers

Give each process a distinct configuration and state file, and ensure every worker has a unique Droplet and Git branch:

```bash
VPS_CODEX_CONFIG="$PWD/config-api.env" ./01-allocate.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./02-provision-github-workspace.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./02-enable-remote-control.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./03-destroy.sh
```

Each config should set a distinct `VPS_STATE_FILE`, `DROPLET_NAME`, and `WORK_BRANCH`.

## Operational hardening

For repeated or unattended use, add an external TTL reaper in a separate trusted environment. It should enumerate resources tagged `codex-agent` and delete expired or orphaned Droplets. Local shell traps cannot clean up after laptop failure, network loss, or a killed process.

Also consider replacing first-use SSH host-key acceptance with an SSH host CA. If this workflow grows beyond a small number of short-lived workers, replace manually created fine-grained PATs with a GitHub App and a trusted token broker; GitHub App installation tokens expire after one hour and cannot be issued with a two-day lifetime.
