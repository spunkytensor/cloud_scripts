#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
load_state
require_command doctl

log "destroying Droplet ${DROPLET_ID} (${DROPLET_NAME:-unknown}, ${DROPLET_IP:-IP pending})"
doctl compute droplet delete "${DROPLET_ID}" --force

rm -f "${KNOWN_HOSTS_FILE:-${VPS_STATE_FILE}.known_hosts}" "${VPS_STATE_FILE}" "${VPS_STATE_FILE}.preserve"
printf 'Destroyed Droplet %s. DigitalOcean compute billing for it has stopped.\n' "${DROPLET_ID}"
