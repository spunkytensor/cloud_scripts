#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
load_state
require_command doctl

if [[ -n "${DROPLET_IP:-}" && -n "${SSH_PRIVATE_KEY_FILE:-}" && -f "${SSH_PRIVATE_KEY_FILE}" ]]; then
  if ( remote_ssh '/usr/local/bin/codex remote-control --json stop >/dev/null 2>&1' ); then
    log "stopped Codex remote control"
  else
    log "remote control was not reachable; continuing with Droplet deletion"
  fi
elif [[ -n "${DROPLET_IP:-}" ]]; then
  log "SSH key is unavailable; skipping optional remote-control stop"
fi

log "destroying Droplet ${DROPLET_ID} (${DROPLET_NAME:-unknown}, ${DROPLET_IP:-IP pending})"
doctl compute droplet delete "${DROPLET_ID}" --force

rm -f \
  "${KNOWN_HOSTS_FILE:-${VPS_STATE_FILE}.known_hosts}" \
  "${SSH_CONFIG_FILE:-${VPS_STATE_FILE}.ssh_config}" \
  "${VPS_STATE_FILE}" \
  "${VPS_STATE_FILE}.preserve" \
  "${VPS_STATE_FILE}.remote-control"
printf 'Destroyed Droplet %s. DigitalOcean compute billing for it has stopped.\n' "${DROPLET_ID}"
