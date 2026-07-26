#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

instance_id=""
confirm_missing_droplet=false
confirm_request_not_accepted=false
while (( $# > 0 )); do
  case "$1" in
    --instance)
      (( $# >= 2 )) || die "--instance requires an instance ID"
      [[ -z "${instance_id}" ]] || die "--instance may only be specified once"
      instance_id="$2"
      shift 2
      ;;
    --confirm-missing-droplet) confirm_missing_droplet=true; shift ;;
    --confirm-request-not-accepted) confirm_request_not_accepted=true; shift ;;
    *) die "usage: ./vps_pause.sh --instance INSTANCE_ID [--confirm-missing-droplet] [--confirm-request-not-accepted]" ;;
  esac
done
[[ -n "${instance_id}" ]] || die "usage: ./vps_pause.sh --instance INSTANCE_ID [--confirm-missing-droplet] [--confirm-request-not-accepted]"

load_config true
select_instance_state "${instance_id}"
require_command doctl
require_command jq
require_command ssh
require_command ssh-keygen
[[ -n "${SSH_PRIVATE_KEY_FILE:-}" && -f "${SSH_PRIVATE_KEY_FILE}" ]] || die "SSH private key is unavailable"

acquire_lifecycle_lock
trap release_lifecycle_lock EXIT

[[ ! -e "${VPS_STATE_FILE}.allocation" ]] || die "instance allocation is unresolved; finish create or destroy it first"
[[ ! -e "${VPS_STATE_FILE}.paused" || -e "${VPS_STATE_FILE}.transition" ]] ||
  die "instance is already paused; resume it with: ./vps_resume.sh --instance ${instance_id}"

if [[ -f "${VPS_STATE_FILE}.transition" ]]; then
  load_transition_state
  [[ "${TRANSITION_KIND}" == pause ]] || die "instance has an unresolved ${TRANSITION_KIND} transition; rerun vps_${TRANSITION_KIND}.sh"
  [[ "${TEARDOWN_STARTED:-0}" != 1 ]] || die "instance teardown has started; finish with vps_destroy.sh"
else
  [[ -f "${VPS_STATE_FILE}" ]] || die "active state not found; create the instance before pausing it"
  load_state
  droplet_json="$(doctl compute droplet get "${DROPLET_ID}" --output json)" || die "could not verify active Droplet ${DROPLET_ID}"
  [[ "$(jq 'length' <<<"${droplet_json}")" == 1 ]] || die "unexpected Droplet lookup response"
  jq -e --argjson id "${DROPLET_ID}" 'length == 1 and .[0].id == $id' <<<"${droplet_json}" >/dev/null ||
    die "provider Droplet identity does not match active state"
  [[ "$(jq '.[0].volume_ids | length' <<<"${droplet_json}")" == 0 ]] ||
    die "pause does not support attached DigitalOcean block-storage volumes"

  TRANSITION_KIND=pause
  TRANSITION_PHASE=pausing-quiescing
  OPERATION_ID="$(date -u +%Y%m%d%H%M%S)-$$-${RANDOM}"
  TRANSITION_STARTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  SOURCE_DROPLET_ID="${DROPLET_ID}"
  SOURCE_DROPLET_NAME="$(jq -er '.[0].name' <<<"${droplet_json}")"
  SOURCE_DROPLET_IP="$(jq -r '.[0].networks.v4[]? | select(.type == "public") | .ip_address' <<<"${droplet_json}" | head -n 1)"
  SOURCE_REGION="$(jq -er '.[0].region.slug' <<<"${droplet_json}")"
  SOURCE_SIZE="$(jq -er '.[0].size_slug' <<<"${droplet_json}")"
  SOURCE_TAGS="$(jq -r '.[0].tags | join(",")' <<<"${droplet_json}")"
  SOURCE_DISK_SIZE="$(jq -er '.[0].disk' <<<"${droplet_json}")"
  REMOTE_QUIESCE_VERIFIED=0
  SHUTDOWN_REQUESTED=0
  POWER_OFF_REQUESTED=0
  SNAPSHOT_NAME="vps-codex-${instance_id}-${SOURCE_DROPLET_ID}-${OPERATION_ID}"
  SNAPSHOT_REQUESTED=0
  SOURCE_DELETE_REQUESTED=0
  SOURCE_DELETE_CONFIRMED=0
  write_transition_state || die "could not persist pause intent"
fi

DROPLET_ID="${SOURCE_DROPLET_ID}"
DROPLET_IP="${SOURCE_DROPLET_IP}"
DROPLET_NAME="${SOURCE_DROPLET_NAME}"
KNOWN_HOSTS_FILE="${VPS_STATE_FILE}.known_hosts"

if [[ "${TRANSITION_PHASE}" == pausing-quiescing ]]; then
  log "quiescing worker; do not write to it from another SSH session"
  # The single-quoted program is expanded by the remote Bash, not locally.
  # shellcheck disable=SC2016
  remote_ssh 'test "$(stat -c '\''%U:%G:%a'\'' /etc/cloud/cloud.cfg.d/99-codex-snapshot-host-key.cfg)" = root:root:644 && grep -Fxq '\''ssh_deletekeys: false'\'' /etc/cloud/cloud.cfg.d/99-codex-snapshot-host-key.cfg' ||
    die "worker predates snapshot-safe SSH host-key preservation; recreate it before using pause"
  quiesce_output="$(remote_exec bash -s -- "${OPERATION_ID}" <<'REMOTE'
set -euo pipefail
operation_id="$1"
marker_dir=/home/agent/.config/vps-codex
marker_file="${marker_dir}/pause.env"
install -d -m 0700 "${marker_dir}"
if [[ -f "${marker_file}" ]]; then
  unset OPERATION_ID SSH_HOST_ED25519_PUBLIC_KEY RUNNING_CONTAINER_IDS HEALTHY_CONTAINER_IDS
  # shellcheck disable=SC1090
  source "${marker_file}"
  [[ "${OPERATION_ID:-}" == "${operation_id}" && "${SSH_HOST_ED25519_PUBLIC_KEY:-}" == ssh-ed25519\ * ]] || {
    echo "existing pause inventory belongs to another or invalid operation" >&2
    exit 1
  }
  printf '%s\n' "${SSH_HOST_ED25519_PUBLIC_KEY}"
  exit 0
fi
/usr/local/bin/codex remote-control --json stop >/dev/null 2>&1 || true
running_ids="$(docker ps -q | tr '\n' ' ')"
healthy_ids="$(docker ps --filter health=healthy -q | tr '\n' ' ')"
if [[ -n "${running_ids// /}" ]]; then
  # shellcheck disable=SC2086
  docker stop --time 120 ${running_ids} >/dev/null
fi
for container_id in ${running_ids}; do
  [[ "$(docker inspect -f '{{.State.Running}}' "${container_id}")" == false ]] || exit 1
done
sync
host_key="$(cat /etc/ssh/ssh_host_ed25519_key.pub)"
temporary="$(mktemp "${marker_file}.tmp.XXXXXX")"
chmod 0600 "${temporary}"
{
  printf 'OPERATION_ID=%q\n' "${operation_id}"
  printf 'SSH_HOST_ED25519_PUBLIC_KEY=%q\n' "${host_key}"
  printf 'RUNNING_CONTAINER_IDS=%q\n' "${running_ids}"
  printf 'HEALTHY_CONTAINER_IDS=%q\n' "${healthy_ids}"
} >"${temporary}"
mv "${temporary}" "${marker_file}"
printf '%s\n' "${host_key}"
REMOTE
)" || die "worker quiescence failed; containers may require operator inspection"
  SSH_HOST_ED25519_PUBLIC_KEY="$(tail -n 1 <<<"${quiesce_output}")"
  [[ "${SSH_HOST_ED25519_PUBLIC_KEY}" == ssh-ed25519\ * ]] || die "could not capture the worker SSH Ed25519 host key"
  learned_key="$(ssh-keygen -F "${SOURCE_DROPLET_IP}" -f "${KNOWN_HOSTS_FILE}" 2>/dev/null | awk '$2 == "ssh-ed25519" { print $2 " " $3; exit }')"
  captured_key="$(awk '{ print $1 " " $2 }' <<<"${SSH_HOST_ED25519_PUBLIC_KEY}")"
  [[ -n "${learned_key}" && "${learned_key}" == "${captured_key}" ]] ||
    die "captured host key does not match the key learned for the active worker"
  REMOTE_QUIESCE_VERIFIED=1
  TRANSITION_PHASE=pausing-shutdown
  write_transition_state
fi

if [[ "${TRANSITION_PHASE}" == pausing-shutdown ]]; then
  [[ "${REMOTE_QUIESCE_VERIFIED}" == 1 ]] || die "refusing provider shutdown before verified quiescence"
  if [[ "${SHUTDOWN_REQUESTED:-0}" == 0 ]]; then
    SHUTDOWN_REQUESTED=1
    write_transition_state
    set +e
    action_response="$(doctl_mutate compute droplet-action shutdown "${SOURCE_DROPLET_ID}" --output json 2>&1)"
    action_status=$?
    set -e
    if (( action_status == 0 )); then
      SHUTDOWN_ACTION_ID="$(jq -er '.[0].id' <<<"${action_response}")" || die "unrecognized shutdown response; rerun to reconcile"
      write_transition_state
    fi
  fi
  if [[ "${SHUTDOWN_REQUESTED:-0}" == 1 && -z "${SHUTDOWN_ACTION_ID:-}" ]]; then
    set +e
    reconciled_action_id="$(reconcile_action_id shutdown "${SOURCE_DROPLET_ID}" "${TRANSITION_STARTED_AT}")"
    reconcile_status=$?
    set -e
    if (( reconcile_status == 0 )); then
      SHUTDOWN_ACTION_ID="${reconciled_action_id}"
      write_transition_state
    elif (( reconcile_status == 3 )); then
      die "multiple shutdown actions match this transition; refusing to choose one"
    elif (( reconcile_status != 2 )); then
      die "could not reconcile the shutdown action"
    fi
  fi
  if [[ -n "${SHUTDOWN_ACTION_ID:-}" ]]; then
    wait_for_action "${SHUTDOWN_ACTION_ID}" shutdown "${SOURCE_DROPLET_ID}" 90
  else
    droplet_json="$(doctl compute droplet get "${SOURCE_DROPLET_ID}" --output json)" || die "shutdown result is uncertain; retry to reconcile"
    if [[ "$(jq -r '.[0].status' <<<"${droplet_json}")" != off ]]; then
      if [[ "${confirm_request_not_accepted}" == true ]]; then
        SHUTDOWN_REQUESTED=0
        write_transition_state
      fi
      die "shutdown request is unresolved and the Droplet is not off; retry later${confirm_request_not_accepted:+}"
    fi
  fi
  for attempt in {1..60}; do
    droplet_json="$(doctl compute droplet get "${SOURCE_DROPLET_ID}" --output json)" || die "could not verify powered-off Droplet"
    [[ "$(jq -r '.[0].status' <<<"${droplet_json}")" != off ]] || break
    if (( attempt == 60 )); then
      if [[ "${POWER_OFF_REQUESTED:-0}" == 0 ]]; then
        POWER_OFF_REQUESTED=1
        write_transition_state
        power_response="$(doctl_mutate compute droplet-action power-off "${SOURCE_DROPLET_ID}" --output json)" || die "hard power-off result is uncertain"
        POWER_OFF_ACTION_ID="$(jq -er '.[0].id' <<<"${power_response}")" || die "unrecognized power-off response"
        write_transition_state
      fi
      if [[ -z "${POWER_OFF_ACTION_ID:-}" ]]; then
        set +e
        reconciled_action_id="$(reconcile_action_id power_off "${SOURCE_DROPLET_ID}" "${TRANSITION_STARTED_AT}")"
        reconcile_status=$?
        set -e
        if (( reconcile_status == 0 )); then
          POWER_OFF_ACTION_ID="${reconciled_action_id}"
          write_transition_state
        elif (( reconcile_status == 2 )) && [[ "${confirm_request_not_accepted}" == true ]]; then
          POWER_OFF_REQUESTED=0
          write_transition_state
          die "cleared the unaccepted power-off intent; rerun pause to submit it"
        elif (( reconcile_status == 3 )); then
          die "multiple power-off actions match this transition; refusing to choose one"
        else
          die "power-off request is unresolved; retry later or confirm it was not accepted"
        fi
      fi
      wait_for_action "${POWER_OFF_ACTION_ID}" power_off "${SOURCE_DROPLET_ID}" 90
      for off_attempt in {1..60}; do
        droplet_json="$(doctl compute droplet get "${SOURCE_DROPLET_ID}" --output json)" || die "could not verify hard power-off"
        [[ "$(jq -r '.[0].status' <<<"${droplet_json}")" != off ]] || break
        (( off_attempt < 60 )) || die "power-off action completed but Droplet did not reach off state"
        sleep 5
      done
    fi
    sleep 5
  done
  TRANSITION_PHASE=pausing-snapshot
  write_transition_state
fi

if [[ "${TRANSITION_PHASE}" == pausing-snapshot ]]; then
  if [[ -z "${SNAPSHOT_ID:-}" ]]; then
    if [[ "${SNAPSHOT_REQUESTED:-0}" == 0 ]]; then
      SNAPSHOT_REQUESTED=1
      write_transition_state
      set +e
      snapshot_response="$(doctl_mutate compute droplet-action snapshot "${SOURCE_DROPLET_ID}" --snapshot-name "${SNAPSHOT_NAME}" --output json 2>&1)"
      snapshot_status=$?
      set -e
      if (( snapshot_status == 0 )); then
        SNAPSHOT_ACTION_ID="$(jq -er '.[0].id' <<<"${snapshot_response}")" || die "unrecognized snapshot response; rerun to reconcile"
        write_transition_state
      fi
    fi
    if [[ -n "${SNAPSHOT_ACTION_ID:-}" ]]; then
      wait_for_action "${SNAPSHOT_ACTION_ID}" snapshot "${SOURCE_DROPLET_ID}" 360
    elif [[ "${SNAPSHOT_REQUESTED:-0}" == 1 ]]; then
      set +e
      reconciled_action_id="$(reconcile_action_id snapshot "${SOURCE_DROPLET_ID}" "${TRANSITION_STARTED_AT}")"
      reconcile_status=$?
      set -e
      if (( reconcile_status == 0 )); then
        SNAPSHOT_ACTION_ID="${reconciled_action_id}"
        write_transition_state
        wait_for_action "${SNAPSHOT_ACTION_ID}" snapshot "${SOURCE_DROPLET_ID}" 360
      elif (( reconcile_status == 3 )); then
        die "multiple snapshot actions match this transition; refusing to choose one"
      elif (( reconcile_status != 2 )); then
        die "could not reconcile the snapshot action"
      fi
    fi
    for attempt in {1..60}; do
      snapshots_json="$(doctl compute snapshot list --resource droplet --output json)" || die "could not list snapshots"
      matches="$(jq --arg name "${SNAPSHOT_NAME}" --arg source "${SOURCE_DROPLET_ID}" --arg region "${SOURCE_REGION}" \
        '[.[] | select(.name == $name and .resource_type == "droplet" and .resource_id == $source and (.regions | index($region) != null))]' <<<"${snapshots_json}")"
      count="$(jq 'length' <<<"${matches}")"
      [[ "${count}" != 1 ]] || break
      [[ "${count}" == 0 ]] || die "multiple snapshots match ${SNAPSHOT_NAME}; refusing to choose one"
      if (( attempt == 60 )); then
        if [[ -z "${SNAPSHOT_ACTION_ID:-}" && "${confirm_request_not_accepted}" == true ]]; then
          SNAPSHOT_REQUESTED=0
          write_transition_state
        fi
        die "snapshot request remains unresolved; retry later"
      fi
      sleep 10
    done
    SNAPSHOT_ID="$(jq -er '.[0].id' <<<"${matches}")"
    SNAPSHOT_CREATED_AT="$(jq -er '.[0].created_at' <<<"${matches}")"
    snapshot_json="$(get_snapshot_json "${SNAPSHOT_ID}")" || die "could not retrieve snapshot ${SNAPSHOT_ID}"
    validate_snapshot_json "${snapshot_json}" "${SNAPSHOT_ID}" "${SNAPSHOT_NAME}" "${SOURCE_DROPLET_ID}" "${SOURCE_REGION}" "${SOURCE_DISK_SIZE}" ||
      die "snapshot ${SNAPSHOT_ID} failed identity or disk-size validation"
    write_transition_state || die "could not durably record verified snapshot; source Droplet retained"
  fi
  TRANSITION_PHASE=pausing-delete-pending
  write_transition_state
fi

if [[ "${TRANSITION_PHASE}" == pausing-delete-pending ]]; then
  [[ "${SNAPSHOT_ID:-}" =~ ^[1-9][0-9]*$ ]] || die "refusing source deletion without a verified snapshot ID"
  rm -f "${VPS_STATE_FILE}.known_hosts" "${VPS_STATE_FILE}.ssh_config"
  if [[ "${SOURCE_DELETE_CONFIRMED:-0}" != 1 ]]; then
    if [[ "${SOURCE_DELETE_REQUESTED:-0}" == 1 ]]; then
      set +e
      lookup_output="$(doctl compute droplet get "${SOURCE_DROPLET_ID}" --output json 2>&1)"
      lookup_status=$?
      set -e
      if (( lookup_status == 0 )); then
        jq -e --argjson id "${SOURCE_DROPLET_ID}" 'length == 1 and .[0].id == $id' <<<"${lookup_output}" >/dev/null ||
          die "source Droplet lookup returned the wrong identity"
        log "source Droplet still exists; safely retrying deletion of exact ID ${SOURCE_DROPLET_ID}"
      elif [[ "$(doctl_http_status <<<"${lookup_output}")" == 404 && "${confirm_missing_droplet}" == true ]]; then
        SOURCE_DELETE_CONFIRMED=1
        write_transition_state
      else
        die "source deletion is unresolved; verify its absence and rerun with --confirm-missing-droplet"
      fi
    fi
    if [[ "${SOURCE_DELETE_CONFIRMED:-0}" != 1 ]]; then
      if [[ "${SOURCE_DELETE_REQUESTED:-0}" == 0 ]]; then
        SOURCE_DELETE_REQUESTED=1
        write_transition_state
      fi
      set +e
      delete_output="$(doctl_mutate compute droplet delete "${SOURCE_DROPLET_ID}" --force 2>&1)"
      delete_status=$?
      set -e
      if (( delete_status == 0 )); then
        SOURCE_DELETE_CONFIRMED=1
        write_transition_state
      else
        [[ -z "${delete_output}" ]] || printf '%s\n' "${delete_output}" >&2
        die "source Droplet deletion was not confirmed; snapshot retained and compute may remain billable"
      fi
    fi
  fi
  write_paused_state || die "source is deleted but paused state could not be committed; transition retained"
  rm -f "${VPS_STATE_FILE}"
  rm -f "${VPS_STATE_FILE}.transition"
  printf 'Paused instance %s as snapshot %s. Droplet compute billing has stopped; snapshot storage remains billable.\n' \
    "${instance_id}" "${SNAPSHOT_ID}"
fi
