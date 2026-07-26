#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

instance_id=""
confirm_missing_snapshot=false
confirm_request_not_accepted=false
while (( $# > 0 )); do
  case "$1" in
    --instance)
      (( $# >= 2 )) || die "--instance requires an instance ID"
      [[ -z "${instance_id}" ]] || die "--instance may only be specified once"
      instance_id="$2"
      shift 2
      ;;
    --confirm-missing-snapshot) confirm_missing_snapshot=true; shift ;;
    --confirm-request-not-accepted) confirm_request_not_accepted=true; shift ;;
    *) die "usage: ./vps_resume.sh --instance INSTANCE_ID [--confirm-missing-snapshot] [--confirm-request-not-accepted]" ;;
  esac
done
[[ -n "${instance_id}" ]] || die "usage: ./vps_resume.sh --instance INSTANCE_ID [--confirm-missing-snapshot] [--confirm-request-not-accepted]"

load_config true
select_instance_state "${instance_id}"
require_command doctl
require_command gh
require_command jq
require_command ssh
[[ -n "${DO_SSH_KEY:-}" ]] || die "DO_SSH_KEY is required in ${CONFIG_FILE}"
[[ -n "${SSH_PRIVATE_KEY_FILE:-}" && -f "${SSH_PRIVATE_KEY_FILE}" ]] || die "SSH private key is unavailable"

acquire_lifecycle_lock
trap release_lifecycle_lock EXIT

[[ ! -e "${VPS_STATE_FILE}.allocation" ]] || die "fresh allocation state conflicts with snapshot resume"
[[ -f "${VPS_STATE_FILE}.setup" ]] || die "repository ownership state is missing; refusing snapshot resume"

if [[ -f "${VPS_STATE_FILE}.transition" ]]; then
  load_transition_state
  [[ "${TRANSITION_KIND}" == resume ]] || die "instance has an unresolved pause transition; rerun vps_pause.sh"
  [[ "${TEARDOWN_STARTED:-0}" != 1 ]] || die "instance teardown has started; finish with vps_destroy.sh"
else
  [[ ! -f "${VPS_STATE_FILE}" ]] || die "instance is already active"
  load_paused_state
  paused_snapshot_id="${SNAPSHOT_ID}"
  paused_snapshot_name="${SNAPSHOT_NAME}"
  paused_source_id="${SNAPSHOT_SOURCE_DROPLET_ID}"
  paused_name="${DROPLET_NAME}"
  paused_region="${DROPLET_REGION}"
  paused_size="${DROPLET_SIZE}"
  paused_tags="${DROPLET_TAGS}"
  paused_disk="${SOURCE_DISK_SIZE:-0}"
  paused_host_key="${SSH_HOST_ED25519_PUBLIC_KEY}"
  paused_operation_id="${PAUSE_OPERATION_ID}"
  snapshot_json="$(get_snapshot_json "${paused_snapshot_id}")" || die "could not retrieve paused snapshot ${paused_snapshot_id}"
  validate_snapshot_json "${snapshot_json}" "${paused_snapshot_id}" "${paused_snapshot_name}" "${paused_source_id}" "${paused_region}" "${paused_disk}" ||
    die "paused snapshot failed identity, region, or disk-size validation"

  TRANSITION_KIND=resume
  TRANSITION_PHASE=resuming-allocation
  OPERATION_ID="$(date -u +%Y%m%d%H%M%S)-$$-${RANDOM}"
  PAUSE_OPERATION_ID="${paused_operation_id}"
  TRANSITION_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  SNAPSHOT_ID="${paused_snapshot_id}"
  SNAPSHOT_NAME="${paused_snapshot_name}"
  SOURCE_DROPLET_ID="${paused_source_id}"
  SOURCE_REGION="${paused_region}"
  SOURCE_DISK_SIZE="${paused_disk}"
  SSH_HOST_ED25519_PUBLIC_KEY="${paused_host_key}"
  TARGET_DROPLET_NAME="${paused_name}"
  TARGET_REGION="${paused_region}"
  TARGET_SIZE="${paused_size}"
  TARGET_TAGS="${paused_tags}"
  TARGET_LIFECYCLE_TAG="codex-resume-${instance_id}-${OPERATION_ID}"
  TARGET_CREATE_REQUESTED=0
  SNAPSHOT_DELETE_REQUESTED=0
  SNAPSHOT_DELETE_CONFIRMED=0
  write_transition_state || die "could not persist resume intent"
fi

if [[ "${TRANSITION_PHASE}" == resuming-allocation ]]; then
  snapshot_json="$(get_snapshot_json "${SNAPSHOT_ID}")" || die "could not retrieve snapshot ${SNAPSHOT_ID}"
  validate_snapshot_json "${snapshot_json}" "${SNAPSHOT_ID}" "${SNAPSHOT_NAME}" "${SOURCE_DROPLET_ID}" "${TARGET_REGION}" "${SOURCE_DISK_SIZE:-0}" ||
    die "snapshot no longer matches the recorded paused machine"

  if [[ -z "${TARGET_DROPLET_ID:-}" ]]; then
    if [[ "${TARGET_CREATE_REQUESTED:-0}" == 0 ]]; then
      TARGET_CREATE_REQUESTED=1
      write_transition_state
      set +e
      create_response="$(doctl_mutate compute droplet create "${TARGET_DROPLET_NAME}" \
        --image "${SNAPSHOT_ID}" \
        --region "${TARGET_REGION}" \
        --size "${TARGET_SIZE}" \
        --ssh-keys "${DO_SSH_KEY}" \
        --tag-names "${TARGET_TAGS:+${TARGET_TAGS},}${TARGET_LIFECYCLE_TAG}" \
        --enable-monitoring \
        --output json 2>&1)"
      create_status=$?
      set -e
      if (( create_status != 0 )); then
        [[ -z "${create_response}" ]] || printf '%s\n' "${create_response}" >&2
      fi
    fi

    matches="$(doctl compute droplet list --tag-name "${TARGET_LIFECYCLE_TAG}" --output json)" || die "could not reconcile replacement allocation"
    count="$(jq 'length' <<<"${matches}")"
    if [[ "${count}" == 0 ]]; then
      if [[ "${confirm_request_not_accepted}" == true ]]; then
        TARGET_CREATE_REQUESTED=0
        write_transition_state
      fi
      die "replacement create request has no visible tagged Droplet; retry later"
    elif [[ "${count}" != 1 ]]; then
      die "multiple Droplets have lifecycle tag ${TARGET_LIFECYCLE_TAG}; refusing to choose one"
    fi
    TARGET_DROPLET_ID="$(jq -er '.[0].id' <<<"${matches}")"
    write_transition_state
  fi

  for attempt in {1..120}; do
    droplet_json="$(doctl compute droplet get "${TARGET_DROPLET_ID}" --output json)" || die "could not retrieve replacement Droplet"
    TARGET_DROPLET_IP="$(jq -r '.[0].networks.v4[]? | select(.type == "public") | .ip_address' <<<"${droplet_json}" | head -n 1)"
    [[ -z "${TARGET_DROPLET_IP}" ]] || break
    (( attempt < 120 )) || die "replacement Droplet has no public IP"
    sleep 5
  done
  jq -e --argjson id "${TARGET_DROPLET_ID}" --arg name "${TARGET_DROPLET_NAME}" \
    --arg region "${TARGET_REGION}" --arg size "${TARGET_SIZE}" --argjson image "${SNAPSHOT_ID}" \
    --arg tag "${TARGET_LIFECYCLE_TAG}" '
      length == 1 and .[0].id == $id and .[0].name == $name and
      .[0].region.slug == $region and .[0].size_slug == $size and
      .[0].image.id == $image and (.[0].tags | index($tag) != null)
    ' <<<"${droplet_json}" >/dev/null || die "replacement Droplet does not match the persisted create recipe"
  write_transition_state

  expected_key="$(awk '{ print $1 " " $2 }' <<<"${SSH_HOST_ED25519_PUBLIC_KEY}")"
  atomic_write_file "${VPS_STATE_FILE}.known_hosts" 0600 <<EOF
${TARGET_DROPLET_IP} ${expected_key}
EOF
  write_state "${TARGET_DROPLET_ID}" "${TARGET_DROPLET_IP}" "${TARGET_DROPLET_NAME}" || die "could not persist replacement active state"
  load_state
  write_ssh_endpoint yes || die "could not install replacement SSH endpoint"
  TRANSITION_PHASE=resuming-recovery
  write_transition_state
fi

if [[ "${TRANSITION_PHASE}" == resuming-recovery ]]; then
  load_state
  SSH_STRICT_HOST_KEY_CHECKING=yes
  export SSH_STRICT_HOST_KEY_CHECKING
  wait_for_worker_boot true
  actual_host_key="$(remote_ssh 'cat /etc/ssh/ssh_host_ed25519_key.pub')" || die "could not verify resumed SSH host key"
  [[ "$(awk '{ print $1 " " $2 }' <<<"${actual_host_key}")" == "$(awk '{ print $1 " " $2 }' <<<"${SSH_HOST_ED25519_PUBLIC_KEY}")" ]] ||
    die "resumed SSH host key does not match the snapshotted machine"

  remote_exec bash -s -- "${PAUSE_OPERATION_ID}" <<'REMOTE'
set -euo pipefail
marker=/home/agent/.config/vps-codex/pause.env
expected_operation_id="$1"
[[ -f "${marker}" ]] || { echo "pause inventory is missing" >&2; exit 1; }
# shellcheck disable=SC1090
source "${marker}"
[[ "${OPERATION_ID}" == "${expected_operation_id}" ]] || { echo "pause inventory belongs to another operation" >&2; exit 1; }
for container_id in ${RUNNING_CONTAINER_IDS:-}; do
  docker inspect "${container_id}" >/dev/null
  docker start "${container_id}" >/dev/null
done
for container_id in ${RUNNING_CONTAINER_IDS:-}; do
  [[ "$(docker inspect -f '{{.State.Running}}' "${container_id}")" == true ]]
done
for attempt in {1..90}; do
  unhealthy=false
  for container_id in ${HEALTHY_CONTAINER_IDS:-}; do
    [[ "$(docker inspect -f '{{.State.Health.Status}}' "${container_id}")" == healthy ]] || unhealthy=true
  done
  [[ "${unhealthy}" == true ]] || exit 0
  (( attempt < 90 )) || exit 1
  sleep 5
done
REMOTE

  unset SETUP_REPOSITORY SETUP_BASE_BRANCH SETUP_WORK_BRANCH SETUP_INSTANCE_ID SETUP_CREDENTIAL_ISOLATION_VERSION
  # shellcheck disable=SC1090
  source "${VPS_STATE_FILE}.setup"
  [[ "${SETUP_INSTANCE_ID:-}" == "${instance_id}" && "${SETUP_CREDENTIAL_ISOLATION_VERSION:-}" == 1 ]] ||
    die "repository ownership marker does not belong to this instance"
  repository="${SETUP_REPOSITORY}"
  checkout_dir="/home/agent/projects/${repository##*/}"
  # The single-quoted program is expanded by the remote Bash, not locally.
  # shellcheck disable=SC2016
  remote_exec bash -c '
    set -euo pipefail
    checkout="$1" expected_origin="$2" expected_branch="$3"
    [[ -d "${checkout}/.git" ]]
    [[ "$(git -C "${checkout}" remote get-url origin)" == "${expected_origin}" ]]
    [[ "$(git -C "${checkout}" symbolic-ref --short HEAD)" == "${expected_branch}" ]]
  ' -- "${checkout_dir}" "https://github.com/${repository}.git" "${SETUP_WORK_BRANCH}" ||
    die "resumed checkout origin or branch does not match its ownership state"

  token_file="${VPS_STATE_FILE}.github-token"
  replacement_file="${token_file}.replacement"
  [[ -s "${token_file}" ]] || die "retained GitHub credential is missing"
  github_token="$(cat "${token_file}")"
  [[ "${github_token}" == github_pat_* ]] || die "retained GitHub credential has an unexpected type"

  install_resume_token() {
    local token="$1"
    # The single-quoted program is expanded by the remote Bash, not locally.
    # shellcheck disable=SC2016
    printf '%s' "${token}" | remote_exec bash -c '
      set -euo pipefail
      token_file=/home/agent/.config/vps-codex/github-token
      candidate="$(mktemp /home/agent/.config/vps-codex/github-token.resume.XXXXXX)"
      trap '\''rm -f "${candidate}"'\'' EXIT
      chmod 0600 "${candidate}"
      cat >"${candidate}"
      install -m 0600 "${candidate}" "${token_file}"
    '
  }

  revoke_resume_token() {
    local token="$1"
    local response_file
    command -v curl >/dev/null 2>&1 || return 1
    response_file="$(mktemp "$(dirname "${token_file}")/github-revoke.XXXXXX")" || return 1
    chmod 0600 "${response_file}" || { rm -f "${response_file}"; return 1; }
    if printf '%s' "${token}" | jq -Rsc '{credentials: [.]}' | curl --fail-with-body --silent --show-error \
      --request POST --header 'Accept: application/vnd.github+json' --header 'Content-Type: application/json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' --data-binary @- --output "${response_file}" \
      https://api.github.com/credentials/revoke; then
      rm -f "${response_file}"
      return 0
    fi
    rm -f "${response_file}"
    return 1
  }

  if [[ -f "${replacement_file}" ]]; then
    unset REPLACEMENT_OLD_GITHUB_TOKEN REPLACEMENT_NEW_GITHUB_TOKEN
    # shellcheck disable=SC1090
    source "${replacement_file}"
    [[ "${REPLACEMENT_OLD_GITHUB_TOKEN:-}" == github_pat_* && "${REPLACEMENT_NEW_GITHUB_TOKEN:-}" == github_pat_* ]] ||
      die "invalid GitHub credential replacement journal"
    GH_TOKEN="${REPLACEMENT_NEW_GITHUB_TOKEN}" gh api "repos/${repository}" >/dev/null 2>&1 ||
      die "pending replacement PAT cannot access ${repository}"
    install_resume_token "${REPLACEMENT_NEW_GITHUB_TOKEN}" || die "could not finish remote PAT replacement"
    atomic_write_file "${token_file}" 0600 <<<"${REPLACEMENT_NEW_GITHUB_TOKEN}" || die "could not save replacement PAT locally"
    github_token="${REPLACEMENT_NEW_GITHUB_TOKEN}"
    if revoke_resume_token "${REPLACEMENT_OLD_GITHUB_TOKEN}"; then
      rm -f "${replacement_file}"
    else
      log "WARNING: old PAT revocation failed; replacement journal retained for teardown"
    fi
  elif ! GH_TOKEN="${github_token}" gh api "repos/${repository}" >/dev/null 2>&1; then
    token_url="https://github.com/settings/personal-access-tokens/new?name=$(printf '%s' "Codex VPS ${instance_id}" | jq -sRr @uri)&target_name=$(printf '%s' "${repository%%/*}" | jq -sRr @uri)&expires_in=2&contents=write&pull_requests=write&actions=read&statuses=read"
    printf '\nThe retained PAT expired while paused. Create a replacement restricted to %s:\n  %s\n' "${repository}" "${token_url}" >&2
    [[ -t 0 ]] || die "PAT replacement requires an interactive terminal; snapshot retained"
    IFS= read -r -s -p "Replacement fine-grained PAT: " new_github_token
    printf '\n' >&2
    [[ "${new_github_token}" == github_pat_* && "${new_github_token}" != "${github_token}" ]] || die "invalid replacement PAT"
    GH_TOKEN="${new_github_token}" gh api "repos/${repository}" >/dev/null 2>&1 || die "replacement PAT cannot access ${repository}"
    atomic_write_file "${replacement_file}" 0600 <<EOF
REPLACEMENT_OLD_GITHUB_TOKEN=$(printf %q "${github_token}")
REPLACEMENT_NEW_GITHUB_TOKEN=$(printf %q "${new_github_token}")
EOF
    install_resume_token "${new_github_token}" || die "replacement journal retained after remote PAT installation failed"
    atomic_write_file "${token_file}" 0600 <<<"${new_github_token}" || die "replacement journal retained after local PAT save failed"
    old_github_token="${github_token}"
    github_token="${new_github_token}"
    if revoke_resume_token "${old_github_token}"; then
      rm -f "${replacement_file}"
    else
      log "WARNING: old PAT revocation failed; replacement journal retained for teardown"
    fi
    unset new_github_token old_github_token
  fi

  # The single-quoted program is expanded by the remote Bash, not locally.
  # shellcheck disable=SC2016
  printf '%s' "${github_token}" | remote_exec bash -c '
    set -euo pipefail
    candidate="$(mktemp)"; trap '\''rm -f "${candidate}"'\'' EXIT
    cat >"${candidate}"
    cmp -s "${candidate}" /home/agent/.config/vps-codex/github-token
    /usr/local/bin/gh auth status >/dev/null
  ' || die "remote GitHub credential does not match the retained host credential"
  unset github_token

  [[ -f "${VPS_STATE_FILE}.remote-control" ]] || die "ChatGPT ownership marker is missing"
  login_status="$(remote_ssh '/usr/local/bin/codex login status')" || die "could not verify ChatGPT login"
  [[ "${login_status}" == *"Logged in using ChatGPT"* ]] || die "snapshotted ChatGPT login is unavailable"
  start_json="$(remote_ssh '/usr/local/bin/codex remote-control --json start')" || die "could not restart Codex remote control"
  jq -e '.mode == "daemon" and (.status == "connected" or .status == "connecting")' <<<"${start_json}" >/dev/null ||
    die "Codex remote control did not reach a usable state"
  pair_json="$(remote_ssh '/usr/local/bin/codex remote-control --json pair')" || die "could not request a pairing code"
  pairing_code="$(jq -er '.manualPairingCode' <<<"${pair_json}")"
  RECOVERY_VERIFIED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  TRANSITION_PHASE=active-snapshot-cleanup-pending
  write_transition_state
fi

if [[ "${TRANSITION_PHASE}" == active-snapshot-cleanup-pending ]]; then
  [[ -n "${RECOVERY_VERIFIED_AT:-}" ]] || die "refusing snapshot deletion before complete worker recovery"
  if [[ "${SNAPSHOT_DELETE_CONFIRMED:-0}" != 1 ]]; then
    if [[ "${SNAPSHOT_DELETE_REQUESTED:-0}" == 1 ]]; then
      set +e
      lookup_output="$(doctl compute snapshot get "${SNAPSHOT_ID}" --output json 2>&1)"
      lookup_status=$?
      set -e
      if (( lookup_status == 0 )); then
        validate_snapshot_json "${lookup_output}" "${SNAPSHOT_ID}" "${SNAPSHOT_NAME}" "${SOURCE_DROPLET_ID}" "${TARGET_REGION}" "${SOURCE_DISK_SIZE:-0}" ||
          die "snapshot cleanup lookup returned the wrong identity"
        log "snapshot still exists; safely retrying deletion of exact ID ${SNAPSHOT_ID}"
      elif [[ "$(doctl_http_status <<<"${lookup_output}")" == 404 && "${confirm_missing_snapshot}" == true ]]; then
        SNAPSHOT_DELETE_CONFIRMED=1
        write_transition_state
      else
        die "snapshot deletion is unresolved; verify its absence and rerun with --confirm-missing-snapshot"
      fi
    fi
    if [[ "${SNAPSHOT_DELETE_CONFIRMED:-0}" != 1 ]]; then
      if [[ "${SNAPSHOT_DELETE_REQUESTED:-0}" == 0 ]]; then
        SNAPSHOT_DELETE_REQUESTED=1
        write_transition_state
      fi
      set +e
      delete_output="$(doctl_mutate compute snapshot delete "${SNAPSHOT_ID}" --force 2>&1)"
      delete_status=$?
      set -e
      if (( delete_status == 0 )); then
        SNAPSHOT_DELETE_CONFIRMED=1
        write_transition_state
      else
        [[ -z "${delete_output}" ]] || printf '%s\n' "${delete_output}" >&2
        die "worker is active, but recovery snapshot cleanup failed; rerun vps_resume.sh"
      fi
    fi
  fi
  load_state
  remote_ssh 'rm -f /home/agent/.config/vps-codex/pause.env' || log "WARNING: could not remove the consumed remote pause marker"
  rm -f "${VPS_STATE_FILE}.paused"
  rm -f "${VPS_STATE_FILE}.transition"
  printf '\nCodex VPS resumed\n  Instance: %s\n  Droplet: %s\n  IP: %s\n  SSH host: %s\n  pairing code: %s\n\nConnect with:\n  ssh -F %q %q\n' \
    "${instance_id}" "${DROPLET_ID}" "${DROPLET_IP}" "${SSH_ALIAS}" "${pairing_code:-request a fresh code with codex remote-control --json pair}" \
    "${SSH_CONFIG_FILE}" "${SSH_ALIAS}"
fi
