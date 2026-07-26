#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

(( $# == 1 )) || die "usage: ./vps_shell.sh INSTANCE_ID"
instance_id="$1"

load_config true
select_instance_state "${instance_id}"
if [[ -f "${VPS_STATE_FILE}.transition" ]]; then
  load_transition_state
  if [[ "${TRANSITION_PHASE}" != active-snapshot-cleanup-pending ]]; then
    die "instance is ${TRANSITION_PHASE}; wait for or resume the lifecycle operation"
  fi
elif [[ -f "${VPS_STATE_FILE}.paused" && -f "${VPS_STATE_FILE}" ]]; then
  die "instance has contradictory active and paused state; inspect or destroy it before connecting"
elif [[ -f "${VPS_STATE_FILE}.paused" ]]; then
  die "instance is paused; resume it with: ./vps_resume.sh --instance ${instance_id}"
fi
unset SSH_ALIAS SSH_CONFIG_FILE
load_state
require_command ssh

[[ -n "${SSH_CONFIG_FILE:-}" ]] || die "SSH config path is missing from ${VPS_STATE_FILE}; resume provisioning with vps_create.sh"
[[ -f "${SSH_CONFIG_FILE}" ]] || die "SSH config not found: ${SSH_CONFIG_FILE}; resume provisioning with vps_create.sh"
[[ -n "${SSH_ALIAS:-}" ]] || die "SSH alias is missing from ${VPS_STATE_FILE}; resume provisioning with vps_create.sh"

exec ssh -F "${SSH_CONFIG_FILE}" "${SSH_ALIAS}"
