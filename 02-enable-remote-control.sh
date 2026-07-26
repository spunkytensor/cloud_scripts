#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
load_state
require_command jq
require_command ssh

remote_codex="/usr/local/bin/codex"
remote_control_marker="${VPS_STATE_FILE}.remote-control"
enrolled_at=""

write_remote_control_marker() {
  local temporary_marker

  temporary_marker="$(mktemp "${remote_control_marker}.tmp.XXXXXX")"
  chmod 0600 "${temporary_marker}"
  {
    printf 'REMOTE_CONTROL_ENROLLED_AT=%q\n' "${enrolled_at}"
    if [[ -n "${server_name:-}" ]]; then
      printf 'REMOTE_CONTROL_SERVER=%q\n' "${server_name}"
      printf 'REMOTE_CONTROL_ENVIRONMENT_ID=%q\n' "${environment_id}"
      printf 'REMOTE_CONTROL_STARTED_AT=%q\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    fi
  } >"${temporary_marker}"
  mv "${temporary_marker}" "${remote_control_marker}"
}

remote_version="$(remote_ssh "${remote_codex} --version" | awk 'NF >= 2 { print $2; exit }')"
[[ -n "${remote_version}" ]] || die "could not determine the remote Codex version"
if ! remote_ssh "dpkg --compare-versions '${remote_version}' ge 0.143.0"; then
  die "Codex ${remote_version} is too old for manual pairing; version 0.143.0 or newer is required"
fi
log "remote Codex ${remote_version} supports manual pairing"

set +e
login_status="$(remote_ssh "${remote_codex} login status" 2>&1)"
login_status_code=$?
set -e

if (( login_status_code == 0 )); then
  [[ "${login_status}" == *"Logged in using ChatGPT"* ]] || die "remote control requires a ChatGPT login, but the VPS reports: ${login_status}"
  [[ -f "${remote_control_marker}" ]] || die "the VPS has an untracked ChatGPT credential; verify its owner, then run 'codex logout' on the VPS before enrolling it through this script"
  # This local mode-0600 file records that this script performed the VPS login.
  # shellcheck disable=SC1090
  source "${remote_control_marker}"
  enrolled_at="${REMOTE_CONTROL_ENROLLED_AT:-${REMOTE_CONTROL_STARTED_AT:-}}"
  [[ -n "${enrolled_at}" ]] || die "remote-control enrollment marker is invalid: ${remote_control_marker}"
elif [[ "${login_status}" == *"Not logged in"* ]]; then
  log "starting the headless ChatGPT device-login flow"
  remote_ssh_tty "${remote_codex} login --device-auth"
else
  die "could not determine remote Codex login status: ${login_status}"
fi

login_status="$(remote_ssh "${remote_codex} login status" 2>&1)"
[[ "${login_status}" == *"Logged in using ChatGPT"* ]] || die "ChatGPT login did not complete: ${login_status}"
if [[ -z "${enrolled_at}" ]]; then
  enrolled_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  write_remote_control_marker
fi

log "starting the detached remote-control daemon"
start_json="$(remote_ssh "${remote_codex} remote-control --json start")"
jq -e '.mode == "daemon" and (.status == "connected" or .status == "connecting")' <<<"${start_json}" >/dev/null || {
  printf '%s\n' "${start_json}" >&2
  die "remote-control daemon did not reach a usable state"
}

environment_id="$(jq -r '.environmentId // "pending"' <<<"${start_json}")"
server_name="$(jq -r '.serverName' <<<"${start_json}")"
write_remote_control_marker

log "requesting a short-lived manual pairing code"
pair_json="$(remote_ssh "${remote_codex} remote-control --json pair")"
pairing_code="$(jq -er '.manualPairingCode' <<<"${pair_json}")"
expires_at="$(jq -r '.expiresAt // "unknown"' <<<"${pair_json}")"

printf '\nHeadless Codex remote control is running\n  server: %s\n  environment: %s\n  pairing code: %s\n  code expires at: %s (Unix time)\n\nApprove this code from the Codex/ChatGPT client using the same account and workspace.\nThe daemon survives SSH logout but not a VPS reboot; rerun this script after reboot.\n' \
  "${server_name}" "${environment_id}" "${pairing_code}" "${expires_at}"
