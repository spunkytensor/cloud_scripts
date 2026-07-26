#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
require_command doctl
require_command jq
require_command ssh
require_command ssh-add
require_command ssh-keygen

[[ -n "${DO_SSH_KEY:-}" ]] || die "DO_SSH_KEY is required in ${CONFIG_FILE}"
[[ -n "${SSH_PRIVATE_KEY_FILE:-}" ]] || die "SSH_PRIVATE_KEY_FILE is required in ${CONFIG_FILE}"
[[ -f "${SSH_PRIVATE_KEY_FILE}" ]] || die "SSH private key not found: ${SSH_PRIVATE_KEY_FILE}"
[[ -f "${SSH_PRIVATE_KEY_FILE}.pub" ]] || die "SSH public key not found: ${SSH_PRIVATE_KEY_FILE}.pub"
[[ ! -e "${VPS_STATE_FILE}" ]] || die "state already exists at ${VPS_STATE_FILE}; destroy that Droplet or choose another VPS_STATE_FILE"

log "validating DigitalOcean SSH key ${DO_SSH_KEY}"
if ! registered_public_key="$(doctl compute ssh-key get "${DO_SSH_KEY}" --output json 2>/dev/null | jq -er '.[0].public_key | split(" ")[0:2] | join(" ")')"; then
  die "DO_SSH_KEY '${DO_SSH_KEY}' was not found; use the numeric ID or fingerprint from: doctl compute ssh-key list"
fi
local_public_key="$(awk 'NF >= 2 { print $1 " " $2; exit }' "${SSH_PRIVATE_KEY_FILE}.pub")"
[[ "${local_public_key}" == "${registered_public_key}" ]] || die "${SSH_PRIVATE_KEY_FILE}.pub does not match DigitalOcean SSH key ${DO_SSH_KEY}"

if ! ssh-keygen -y -P "" -f "${SSH_PRIVATE_KEY_FILE}" >/dev/null 2>&1 &&
  ! ssh-add -L 2>/dev/null | awk 'NF >= 2 { print $1 " " $2 }' | grep -Fxq "${local_public_key}"; then
  die "encrypted SSH key is not loaded; run: ssh-add --apple-use-keychain ${SSH_PRIVATE_KEY_FILE}"
fi

state_dir="$(dirname "${VPS_STATE_FILE}")"
install -d -m 0700 "${state_dir}" || die "cannot create state directory: ${state_dir}"
state_probe="$(mktemp "${VPS_STATE_FILE}.probe.XXXXXX")" || die "state directory is not writable: ${state_dir}"
rm -f "${state_probe}"
cloud_init_file="$(mktemp "${state_dir}/cloud-init.XXXXXX.yaml")" || die "cannot create rendered cloud-init file"
chmod 0600 "${cloud_init_file}"
trap 'rm -f "${cloud_init_file}"' EXIT
sed "s|__AGENT_SSH_AUTHORIZED_KEY__|${registered_public_key}|" "${SCRIPT_DIR}/cloud-init.yaml" >"${cloud_init_file}"

DO_REGION="${DO_REGION:-nyc3}"
DO_SIZE="${DO_SIZE:-s-4vcpu-8gb}"
DO_IMAGE="${DO_IMAGE:-ubuntu-24-04-x64}"
DO_TAGS="${DO_TAGS:-codex-agent}"
DROPLET_NAME="${DROPLET_NAME:-codex-agent-$(date +%Y%m%d-%H%M%S)}"

log "requesting ${DROPLET_NAME} (${DO_SIZE}, ${DO_REGION}, ${DO_IMAGE}) from DigitalOcean"
if ! response="$({
  doctl compute droplet create "${DROPLET_NAME}" \
    --region "${DO_REGION}" \
    --size "${DO_SIZE}" \
    --image "${DO_IMAGE}" \
    --ssh-keys "${DO_SSH_KEY}" \
    --tag-names "${DO_TAGS}" \
    --enable-monitoring \
    --user-data-file "${cloud_init_file}" \
    --output json
} )"; then
  die "DigitalOcean rejected the create request; no state file was written and no Droplet ID was received"
fi

droplet_id="$(jq -er '.[0].id' <<<"${response}")"
if ! write_state "${droplet_id}" "" "${DROPLET_NAME}"; then
  log "could not persist state for created Droplet ${droplet_id}; attempting immediate deletion"
  if ! doctl compute droplet delete "${droplet_id}" --force; then
    printf 'CRITICAL: failed to record or delete billable Droplet %s; delete it manually with doctl.\n' "${droplet_id}" >&2
  fi
  die "could not write provisional Droplet state"
fi

log "DigitalOcean accepted the request as Droplet ${droplet_id}; cleanup state saved to ${VPS_STATE_FILE}"
log "waiting for DigitalOcean to assign a public IP"
droplet_ip=""
for attempt in {1..60}; do
  droplet_ip="$(doctl compute droplet get "${droplet_id}" --output json | jq -r '.[0].networks.v4[]? | select(.type == "public") | .ip_address' | head -n 1)"
  if [[ -n "${droplet_ip}" ]]; then
    break
  fi
  if (( attempt == 60 )); then
    die "Droplet did not receive a public IP; provisional state remains in ${VPS_STATE_FILE} for cleanup"
  fi
  sleep 5
done

write_state "${droplet_id}" "${droplet_ip}" "${DROPLET_NAME}"
load_state

install -m 0600 /dev/null "${SSH_CONFIG_FILE}"
cat >"${SSH_CONFIG_FILE}" <<EOF
Host ${SSH_ALIAS}
  HostName ${DROPLET_IP}
  User ${REMOTE_SSH_USER}
  IdentityFile "${SSH_PRIVATE_KEY_FILE}"
  IdentitiesOnly yes
  UserKnownHostsFile "${KNOWN_HOSTS_FILE}"
  StrictHostKeyChecking accept-new
EOF

log "Droplet ${DROPLET_ID} is ${DROPLET_IP}; waiting for SSH as ${REMOTE_SSH_USER}"
for attempt in {1..120}; do
  if remote_ssh true >/dev/null 2>&1; then
    break
  fi
  if (( attempt == 120 )); then
    die "SSH for ${REMOTE_SSH_USER} did not become ready; Droplet state remains in ${VPS_STATE_FILE} for cleanup"
  fi
  sleep 5
done

log "waiting for cloud-init (this installs Docker, Node.js, GitHub CLI, and Codex)"
remote_ssh 'cloud-init status --wait >/dev/null && test -f /opt/codex-worker-ready'

printf 'Droplet ready\n  id: %s\n  ip: %s\n  state: %s\n  SSH alias: %s\n  SSH config: %s\n\nConnect with:\n  ssh -F %q %q\n\nProvision a standalone GitHub workspace with:\n  ./02-provision-github-workspace.sh\n\nEnable headless Codex remote control with:\n  ./02-enable-remote-control.sh\n' \
  "${DROPLET_ID}" "${DROPLET_IP}" "${VPS_STATE_FILE}" "${SSH_ALIAS}" "${SSH_CONFIG_FILE}" \
  "${SSH_CONFIG_FILE}" "${SSH_ALIAS}"
