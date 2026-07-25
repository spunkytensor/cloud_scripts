# Disposable DigitalOcean Codex worker

These scripts create a DigitalOcean Droplet, run a complete Codex development job against a GitHub repository using your existing Codex ChatGPT-account credentials, push the resulting branch, open a draft pull request, and destroy the Droplet.

The remote Codex process operates directly on the VPS checkout. The local scripts are the control plane.

## Security and cost model

- A Droplet is billable until `03-destroy.sh` successfully deletes it. Powering it off is not enough.
- `demo.sh` destroys the Droplet on success or failure unless `KEEP_VPS=1` is set. If a possibly rotated Codex credential cannot be retrieved safely, it instead marks and preserves the billable VPS so you can recover the credential before deletion.
- The scripts copy `~/.codex/auth.json` into the disposable VPS with mode `0600`. The GitHub token is sent only for the clone and publish phases and is deleted before Codex or repository-controlled commands run. Neither credential is placed in cloud-init, the Droplet image, or command arguments.
- Codex may refresh its account token. `02-run-codex-job.sh` retrieves the potentially updated auth file and atomically synchronizes it back only if the local file has not changed concurrently.
- Do not run another Codex process using the same file-backed credentials during a remote job. For production automation, prefer an OpenAI API key, or a Codex access token where available, over copying personal account credentials.
- The Codex process necessarily has access to its own account credential. A malicious repository instruction or command could attempt to print it into the captured job log. Use this personal-account demonstration only with repositories you trust; a dedicated API credential and proxy boundary are safer for unattended production use.
- The remote agent receives `workspace-write` filesystem access and outbound network access. Docker is installed for future controlled use, but the agent is deliberately not in the root-equivalent `docker` group while its Codex credentials live on the host. It does not receive DigitalOcean credentials.
- Cloud-init authorizes the configured shared SSH key for `agent` and disables root SSH login. All post-provisioning SSH and SCP operations, including `02-run-codex-job.sh`, run directly as the unprivileged `agent` user.
- The GitHub token must be able to clone the repository, push the work branch, and create a draft PR. Use the narrowest practical repository-scoped credential for ongoing use.
- SSH bootstrap uses trust on first use (`StrictHostKeyChecking=accept-new`) with a dedicated known-hosts file. This is convenient for an ephemeral demo but does not protect the first connection from an active network attacker. Use an SSH host CA or another independently verified host key before using this pattern in a hostile network.

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

Codex must use file-backed authentication at `~/.codex/auth.json`. This checkout already has such a file if the following succeeds:

```bash
test -s ~/.codex/auth.json && jq -e . ~/.codex/auth.json >/dev/null
```

If Codex currently stores credentials only in the macOS keychain, set this in `~/.codex/config.toml` and sign in again:

```toml
cli_auth_credentials_store = "file"
```

```bash
codex logout
codex login
```

Treat `~/.codex/auth.json` as a password. Never commit or manually paste it into a prompt.

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
- `TASK_FILE`
- `PR_TITLE`

`REPOSITORY` can be `owner/repository` or a GitHub URL. The included example task implements a backend health endpoint and React status UI, adapting to the target repository's existing architecture.

## Run the three lifecycle steps

### 1. Allocate and bootstrap the VPS

```bash
./01-allocate.sh
```

The script records the Droplet ID, IP, and a dedicated SSH known-hosts file under `.state/`. If provisioning fails after DigitalOcean creates the machine, this state remains available for cleanup.

After provisioning, connect directly as the unprivileged user:

```bash
source config.env
source .state/current.env
ssh -i "$SSH_PRIVATE_KEY_FILE" agent@"$DROPLET_IP"
```

Remote root login is disabled.

### 2. Execute the Codex job and stage a draft PR

```bash
./02-run-codex-job.sh
```

The remote worker:

1. Clones `REPOSITORY` at `BASE_BRANCH`.
2. Creates `WORK_BRANCH`.
3. Deletes the GitHub credential before running repository code.
4. Runs `codex exec` with the configured task.
5. Lets Codex inspect, implement, and test the feature.
6. Commits any remaining working-tree changes.
7. Retrieves and validates potentially refreshed Codex credentials.
8. Re-sends the GitHub credential, pushes the branch, and deletes it again.
9. Opens a draft GitHub pull request.
10. Downloads the structured result and Codex report into `.state/results/`.

If credential synchronization is uncertain, read the generated `AUTH-RECOVERY.txt`. `demo.sh` will leave the VPS running and billable rather than destroy the only potentially valid refresh token. After recovery, run `03-destroy.sh` explicitly.

### 3. Destroy the VPS

```bash
./03-destroy.sh
```

Run this even if the job fails. It deletes the Droplet rather than merely powering it off.

## One-command demonstration

After configuring the repository and task, run all three stages with automatic cleanup:

```bash
./demo.sh
```

For troubleshooting only, preserve the VPS after the run:

```bash
KEEP_VPS=1 ./demo.sh
```

That leaves a billable resource. Destroy it explicitly afterward:

```bash
./03-destroy.sh
```

## Multiple concurrent workers

Give each process a distinct configuration and state file, and ensure every worker has a unique Droplet and Git branch:

```bash
VPS_CODEX_CONFIG="$PWD/config-api.env" ./01-allocate.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./02-run-codex-job.sh
VPS_CODEX_CONFIG="$PWD/config-api.env" ./03-destroy.sh
```

Each config should set a distinct `VPS_STATE_FILE`, `DROPLET_NAME`, and `WORK_BRANCH`.

## Operational hardening beyond the demo

For repeated or unattended use, add an external TTL reaper in a separate trusted environment. It should enumerate resources tagged `codex-agent` and delete expired or orphaned Droplets. Local shell traps cannot clean up after laptop failure, network loss, or a killed process.

Also consider replacing first-use SSH host-key acceptance with an SSH host CA, and replacing broad personal GitHub credentials with a GitHub App installation token scoped to one repository.
