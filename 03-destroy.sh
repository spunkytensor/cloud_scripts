#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config

token_state_file="${VPS_STATE_FILE}.github-token"

revoke_github_token() {
  local response_file
  local github_token

  [[ -s "${token_state_file}" ]] || return 0
  if ! command -v curl >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
    log "WARNING: curl and jq are required to revoke the GitHub credential"
    return 1
  fi
  if ! github_token="$(cat "${token_state_file}")"; then
    log "WARNING: could not read the retained GitHub credential at ${token_state_file}"
    return 1
  fi
  if ! response_file="$(mktemp "$(dirname "${token_state_file}")/github-revoke.XXXXXX")"; then
    log "WARNING: could not create a protected GitHub revocation response file"
    return 1
  fi
  if ! chmod 0600 "${response_file}"; then
    rm -f "${response_file}"
    log "WARNING: could not protect the GitHub revocation response file"
    return 1
  fi

  log "revoking the VPS-specific GitHub credential"
  if printf '%s' "${github_token}" | jq -Rsc '{credentials: [.]}' | \
    curl --fail-with-body --silent --show-error \
      --request POST \
      --header 'Accept: application/vnd.github+json' \
      --header 'Content-Type: application/json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --data-binary @- \
      --output "${response_file}" \
      https://api.github.com/credentials/revoke; then
    rm -f "${response_file}" "${token_state_file}"
    log "revoked the VPS-specific GitHub credential"
    return 0
  fi

  rm -f "${response_file}"
  log "WARNING: GitHub credential revocation failed; the protected token remains at ${token_state_file} for retry and will expire on its configured date"
  return 1
}

if [[ ! -f "${VPS_STATE_FILE}" ]]; then
  if [[ -s "${token_state_file}" ]]; then
    revoke_github_token || die "credential revocation failed; rerun 03-destroy.sh to retry"
    printf 'Revoked the retained GitHub credential; no Droplet state existed.\n'
    exit 0
  fi
  die "state file not found: ${VPS_STATE_FILE}; run 01-allocate.sh first"
fi

load_state

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
delete_status=0
if ! command -v doctl >/dev/null 2>&1; then
  log "WARNING: required command not found: doctl"
  delete_status=1
elif ! doctl compute droplet delete "${DROPLET_ID}" --force; then
  log "WARNING: DigitalOcean did not confirm deletion of Droplet ${DROPLET_ID}"
  delete_status=1
fi

revoke_status=0
revoke_github_token || revoke_status=$?

if (( delete_status == 0 )); then
  rm -f \
    "${KNOWN_HOSTS_FILE:-${VPS_STATE_FILE}.known_hosts}" \
    "${SSH_CONFIG_FILE:-${VPS_STATE_FILE}.ssh_config}" \
    "${VPS_STATE_FILE}" \
    "${VPS_STATE_FILE}.remote-control"
  printf 'Destroyed Droplet %s. DigitalOcean compute billing for it has stopped.\n' "${DROPLET_ID}"
else
  printf 'WARNING: Droplet %s may still be running and billable; rerun ./03-destroy.sh.\n' "${DROPLET_ID}" >&2
fi
if (( revoke_status != 0 )); then
  printf 'WARNING: GitHub credential revocation must be retried with ./03-destroy.sh.\n' >&2
fi
(( delete_status == 0 && revoke_status == 0 ))
