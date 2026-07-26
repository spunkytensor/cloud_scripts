#!/usr/bin/env bash

set -euo pipefail

VPS_CODEX_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export VPS_CODEX_DIR

CONFIG_FILE="${VPS_CODEX_CONFIG:-${VPS_CODEX_DIR}/config.env}"
readonly REMOTE_SSH_USER="agent"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[vps-codex] %s\n' "$*" >&2
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

doctl_http_status() {
  sed -nE '
    s/.*: ([0-9]{3}) \(request .*/\1/
    t found
    s/^Error: [A-Z]+ [^[:space:]]+: ([0-9]{3})([[:space:]].*)?$/\1/
    t found
    b
    :found
    p
    q
  '
}

acquire_lifecycle_lock() {
  local lock_dir
  local lock_pid
  local manual_cleanup
  local recovery_dir

  lock_dir="${VPS_STATE_FILE}.lock"
  recovery_dir="${lock_dir}.recovery"
  manual_cleanup="after confirming no create/destroy process is running, clear it with: rm -f '${lock_dir}/pid' && rmdir '${lock_dir}'"
  install -d -m 0700 "$(dirname "${VPS_STATE_FILE}")"

  [[ ! -d "${recovery_dir}" ]] || die "another process is recovering the lifecycle lock at ${lock_dir}; retry shortly"
  if mkdir -m 0700 "${lock_dir}" 2>/dev/null; then
    # A recovery process may have started after the check above. Do not claim
    # the lock while it is serializing stale-owner removal.
    if [[ -d "${recovery_dir}" ]]; then
      rmdir "${lock_dir}" 2>/dev/null || true
      die "another process is recovering the lifecycle lock at ${lock_dir}; retry shortly"
    fi
  else
    if ! mkdir -m 0700 "${recovery_dir}" 2>/dev/null; then
      die "lifecycle lock exists at ${lock_dir}; another create/destroy process may be running; ${manual_cleanup}"
    fi

    lock_pid=""
    if [[ -r "${lock_dir}/pid" ]]; then
      lock_pid="$(cat "${lock_dir}/pid" 2>/dev/null || true)"
    fi
    if [[ -z "${lock_pid}" ]]; then
      rmdir "${recovery_dir}" 2>/dev/null || true
      die "lifecycle lock at ${lock_dir} has no owner PID; refusing to steal a lock whose creator may still be starting; retry first, then ${manual_cleanup}"
    fi
    if [[ "${lock_pid}" =~ ^[1-9][0-9]*$ ]] && kill -0 "${lock_pid}" 2>/dev/null; then
      rmdir "${recovery_dir}" 2>/dev/null || true
      die "lifecycle lock at ${lock_dir} is owned by live process ${lock_pid}"
    fi
    if [[ -n "${lock_pid}" && ! "${lock_pid}" =~ ^[1-9][0-9]*$ ]]; then
      rmdir "${recovery_dir}" 2>/dev/null || true
      die "lifecycle lock at ${lock_dir} has unreadable owner metadata; ${manual_cleanup}"
    fi

    rm -f "${lock_dir}/pid"
    if ! rmdir "${lock_dir}" 2>/dev/null || ! mkdir -m 0700 "${lock_dir}" 2>/dev/null; then
      rmdir "${recovery_dir}" 2>/dev/null || true
      die "could not safely recover stale lifecycle lock at ${lock_dir}"
    fi
    log "recovered stale lifecycle lock${lock_pid:+ from process ${lock_pid}}"
    rmdir "${recovery_dir}" 2>/dev/null || true
  fi
  LIFECYCLE_LOCK_DIR="${lock_dir}"
  printf '%s\n' "$$" >"${LIFECYCLE_LOCK_DIR}/pid" || {
    rmdir "${LIFECYCLE_LOCK_DIR}" 2>/dev/null || true
    die "could not record lifecycle lock ownership"
  }
}

release_lifecycle_lock() {
  local cleanup_dir
  local lock_pid

  [[ -n "${LIFECYCLE_LOCK_DIR:-}" ]] || return 0
  [[ -r "${LIFECYCLE_LOCK_DIR}/pid" ]] || return 0
  lock_pid="$(cat "${LIFECYCLE_LOCK_DIR}/pid")"
  [[ "${lock_pid}" == "$$" ]] || return 0
  cleanup_dir="${LIFECYCLE_LOCK_DIR}.release.$$"
  if mv "${LIFECYCLE_LOCK_DIR}" "${cleanup_dir}" 2>/dev/null; then
    rm -f "${cleanup_dir}/pid"
    rmdir "${cleanup_dir}" 2>/dev/null || true
  fi
}

load_config() {
  local allow_legacy_source_settings="${1:-false}"
  local legacy_source_settings=false

  [[ -f "${CONFIG_FILE}" ]] || die "missing ${CONFIG_FILE}; copy config.example.env to config.env and edit it"
  unset BASE_BRANCH DROPLET_NAME GITHUB_TOKEN_FILE REPOSITORY VPS_INSTANCE_ID VPS_INSTANCE_STATE_DIR VPS_STATE_FILE WORK_BRANCH
  # This is a user-owned shell configuration file and may intentionally use
  # expansions such as $(date ...) for unique resource names.
  # shellcheck disable=SC1090
  source "${CONFIG_FILE}"

  [[ ! "${BASE_BRANCH+x}" && ! "${REPOSITORY+x}" ]] || legacy_source_settings=true
  unset BASE_BRANCH REPOSITORY
  if [[ "${allow_legacy_source_settings}" != true && "${legacy_source_settings}" == true ]]; then
    die "obsolete setting found in ${CONFIG_FILE}; remove BASE_BRANCH and REPOSITORY; pass OWNER/REPOSITORY,BASE_BRANCH to vps_create.sh"
  fi
  [[ ! "${DROPLET_NAME+x}" && ! "${GITHUB_TOKEN_FILE+x}" && ! "${VPS_STATE_FILE+x}" && ! "${WORK_BRANCH+x}" ]] ||
    die "obsolete per-instance setting found in ${CONFIG_FILE}; remove DROPLET_NAME, GITHUB_TOKEN_FILE, VPS_STATE_FILE, and WORK_BRANCH"
  VPS_INSTANCE_STATE_DIR="${VPS_INSTANCE_STATE_DIR:-${VPS_CODEX_DIR}/.state/instances}"
  VPS_INSTANCE_ID=""
  export VPS_INSTANCE_ID VPS_INSTANCE_STATE_DIR
}

instance_state_root() {
  printf '%s\n' "${VPS_INSTANCE_STATE_DIR}"
}

select_instance_state() {
  local instance_id="$1"

  [[ "${instance_id}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "instance ID must contain only letters, numbers, dots, underscores, and hyphens"
  VPS_INSTANCE_ID="${instance_id}"
  VPS_STATE_FILE="$(instance_state_root)/${instance_id}/current.env"
  export VPS_INSTANCE_ID VPS_STATE_FILE
}

load_state() {
  [[ -f "${VPS_STATE_FILE}" ]] || die "state file not found: ${VPS_STATE_FILE}"
  # State is generated by write_state with shell-escaped values.
  # shellcheck disable=SC1090
  source "${VPS_STATE_FILE}"
  [[ -n "${DROPLET_ID:-}" ]] || die "invalid state file: ${VPS_STATE_FILE}"
}

write_state() {
  local droplet_id="$1"
  local droplet_ip="$2"
  local droplet_name="$3"
  local state_dir
  local temporary_state

  state_dir="$(dirname "${VPS_STATE_FILE}")"
  install -d -m 0700 "${state_dir}" || return 1
  temporary_state="$(mktemp "${VPS_STATE_FILE}.tmp.XXXXXX")" || return 1
  chmod 0600 "${temporary_state}" || {
    rm -f "${temporary_state}"
    return 1
  }
  {
    printf 'DROPLET_ID=%q\n' "${droplet_id}"
    printf 'DROPLET_IP=%q\n' "${droplet_ip}"
    printf 'DROPLET_NAME=%q\n' "${droplet_name}"
    printf 'KNOWN_HOSTS_FILE=%q\n' "${VPS_STATE_FILE}.known_hosts"
    printf 'SSH_CONFIG_FILE=%q\n' "${VPS_STATE_FILE}.ssh_config"
    printf 'SSH_ALIAS=%q\n' "$(stable_ssh_alias)"
    printf 'CREATED_AT=%q\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"${temporary_state}" || {
    rm -f "${temporary_state}"
    return 1
  }
  mv "${temporary_state}" "${VPS_STATE_FILE}" || {
    rm -f "${temporary_state}"
    return 1
  }
}

stable_ssh_alias() {
  printf 'codex-vps-%s\n' "${VPS_INSTANCE_ID}"
}

atomic_write_file() {
  local destination="$1"
  local mode="$2"
  local temporary

  install -d -m 0700 "$(dirname "${destination}")" || return 1
  temporary="$(mktemp "${destination}.tmp.XXXXXX")" || return 1
  chmod "${mode}" "${temporary}" || { rm -f "${temporary}"; return 1; }
  cat >"${temporary}" || { rm -f "${temporary}"; return 1; }
  mv "${temporary}" "${destination}" || { rm -f "${temporary}"; return 1; }
}

write_ssh_endpoint() {
  local strict_mode="${1:-accept-new}"

  [[ -n "${DROPLET_IP:-}" && -n "${SSH_PRIVATE_KEY_FILE:-}" ]] || return 1
  SSH_ALIAS="$(stable_ssh_alias)"
  KNOWN_HOSTS_FILE="${VPS_STATE_FILE}.known_hosts"
  SSH_CONFIG_FILE="${VPS_STATE_FILE}.ssh_config"
  atomic_write_file "${SSH_CONFIG_FILE}" 0600 <<EOF
Host ${SSH_ALIAS}
  HostName ${DROPLET_IP}
  User ${REMOTE_SSH_USER}
  IdentityFile "${SSH_PRIVATE_KEY_FILE}"
  IdentitiesOnly yes
  UserKnownHostsFile "${KNOWN_HOSTS_FILE}"
  StrictHostKeyChecking ${strict_mode}
EOF
}

wait_for_worker_boot() {
  local require_prior_boot="${1:-false}"
  local attempt

  log "waiting for SSH as ${REMOTE_SSH_USER}"
  for attempt in {1..120}; do
    if remote_ssh true >/dev/null 2>&1; then
      break
    fi
    (( attempt < 120 )) || die "SSH for ${REMOTE_SSH_USER} did not become ready"
    sleep 5
  done
  if [[ "${require_prior_boot}" == true ]]; then
    remote_ssh 'test -f /opt/codex-worker-ready && cloud-init status >/dev/null' ||
      die "resumed worker is missing its completed bootstrap marker"
  else
    remote_ssh 'cloud-init status --wait >/dev/null && test -f /opt/codex-worker-ready' ||
      die "worker bootstrap did not complete"
  fi
  remote_ssh 'id -nG | tr " " "\n" | grep -Fxq docker && docker info >/dev/null' ||
    die "agent does not have working Docker access"
}

load_paused_state() {
  local paused_file="${VPS_STATE_FILE}.paused"

  [[ -f "${paused_file}" ]] || die "paused state file not found: ${paused_file}"
  unset PAUSED_STATE_VERSION SNAPSHOT_ID SNAPSHOT_NAME SNAPSHOT_SOURCE_DROPLET_ID
  unset SNAPSHOT_CREATED_AT DROPLET_NAME DROPLET_REGION DROPLET_SIZE DROPLET_TAGS
  unset SSH_HOST_ED25519_PUBLIC_KEY PAUSED_AT SOURCE_DISK_SIZE PAUSE_OPERATION_ID
  # Generated by write_paused_state with shell-escaped values.
  # shellcheck disable=SC1090
  source "${paused_file}"
  [[ "${PAUSED_STATE_VERSION:-}" == 1 ]] || die "invalid paused state version: ${paused_file}"
  [[ "${SNAPSHOT_ID:-}" =~ ^[1-9][0-9]*$ && "${SNAPSHOT_SOURCE_DROPLET_ID:-}" =~ ^[1-9][0-9]*$ ]] ||
    die "invalid snapshot identity in ${paused_file}"
  [[ -n "${SNAPSHOT_NAME:-}" && -n "${DROPLET_NAME:-}" && -n "${DROPLET_REGION:-}" &&
    -n "${DROPLET_SIZE:-}" && -n "${SSH_HOST_ED25519_PUBLIC_KEY:-}" && -n "${PAUSE_OPERATION_ID:-}" ]] ||
    die "incomplete paused state: ${paused_file}"
}

write_paused_state() {
  atomic_write_file "${VPS_STATE_FILE}.paused" 0600 <<EOF
PAUSED_STATE_VERSION=1
PAUSE_OPERATION_ID=$(printf %q "${OPERATION_ID}")
SNAPSHOT_ID=$(printf %q "${SNAPSHOT_ID}")
SNAPSHOT_NAME=$(printf %q "${SNAPSHOT_NAME}")
SNAPSHOT_SOURCE_DROPLET_ID=$(printf %q "${SOURCE_DROPLET_ID}")
SNAPSHOT_CREATED_AT=$(printf %q "${SNAPSHOT_CREATED_AT:-}")
DROPLET_NAME=$(printf %q "${SOURCE_DROPLET_NAME}")
DROPLET_REGION=$(printf %q "${SOURCE_REGION}")
DROPLET_SIZE=$(printf %q "${SOURCE_SIZE}")
DROPLET_TAGS=$(printf %q "${SOURCE_TAGS:-}")
SOURCE_DISK_SIZE=$(printf %q "${SOURCE_DISK_SIZE:-0}")
SSH_HOST_ED25519_PUBLIC_KEY=$(printf %q "${SSH_HOST_ED25519_PUBLIC_KEY}")
PAUSED_AT=$(printf %q "$(date -u +%Y-%m-%dT%H:%M:%SZ)")
EOF
}

load_transition_state() {
  local transition_file="${VPS_STATE_FILE}.transition"
  local variable value

  [[ -f "${transition_file}" ]] || die "transition state file not found: ${transition_file}"
  unset TRANSITION_VERSION TRANSITION_KIND TRANSITION_PHASE OPERATION_ID TRANSITION_STARTED_AT
  unset SOURCE_DROPLET_ID SOURCE_DROPLET_NAME SOURCE_DROPLET_IP SOURCE_REGION SOURCE_SIZE SOURCE_TAGS SOURCE_DISK_SIZE
  unset REMOTE_QUIESCE_VERIFIED SHUTDOWN_REQUESTED SHUTDOWN_ACTION_ID POWER_OFF_REQUESTED POWER_OFF_ACTION_ID
  unset SNAPSHOT_NAME SNAPSHOT_REQUESTED SNAPSHOT_ACTION_ID SNAPSHOT_ID SNAPSHOT_CREATED_AT
  unset SOURCE_DELETE_REQUESTED SOURCE_DELETE_CONFIRMED SSH_HOST_ED25519_PUBLIC_KEY
  unset TARGET_DROPLET_NAME TARGET_REGION TARGET_SIZE TARGET_TAGS TARGET_LIFECYCLE_TAG TARGET_CREATE_REQUESTED
  unset TARGET_DROPLET_ID TARGET_DROPLET_IP RECOVERY_VERIFIED_AT SNAPSHOT_DELETE_REQUESTED SNAPSHOT_DELETE_CONFIRMED
  unset PAUSE_OPERATION_ID TEARDOWN_STARTED TARGET_DELETE_CONFIRMED
  # Generated by write_transition_state with shell-escaped values.
  # shellcheck disable=SC1090
  source "${transition_file}"
  [[ "${TRANSITION_VERSION:-}" == 1 ]] || die "invalid transition version: ${transition_file}"
  [[ "${TRANSITION_KIND:-}" == pause || "${TRANSITION_KIND:-}" == resume ]] || die "invalid transition kind: ${transition_file}"
  [[ -n "${TRANSITION_PHASE:-}" && -n "${OPERATION_ID:-}" && -n "${TRANSITION_STARTED_AT:-}" ]] ||
    die "incomplete transition state: ${transition_file}"
  case "${TRANSITION_KIND}:${TRANSITION_PHASE}" in
    pause:pausing-quiescing|pause:pausing-shutdown|pause:pausing-snapshot|pause:pausing-delete-pending|\
    resume:resuming-allocation|resume:resuming-recovery|resume:active-snapshot-cleanup-pending) ;;
    *) die "invalid transition phase ${TRANSITION_PHASE} for ${TRANSITION_KIND}" ;;
  esac
  for variable in REMOTE_QUIESCE_VERIFIED SHUTDOWN_REQUESTED POWER_OFF_REQUESTED SNAPSHOT_REQUESTED \
    SOURCE_DELETE_REQUESTED SOURCE_DELETE_CONFIRMED TARGET_CREATE_REQUESTED SNAPSHOT_DELETE_REQUESTED \
    SNAPSHOT_DELETE_CONFIRMED TEARDOWN_STARTED TARGET_DELETE_CONFIRMED; do
    eval "value=\${${variable}:-}"
    [[ -z "${value}" || "${value}" == 0 || "${value}" == 1 ]] || die "invalid boolean ${variable} in ${transition_file}"
  done
  for variable in SOURCE_DROPLET_ID SHUTDOWN_ACTION_ID POWER_OFF_ACTION_ID SNAPSHOT_ACTION_ID SNAPSHOT_ID TARGET_DROPLET_ID; do
    eval "value=\${${variable}:-}"
    [[ -z "${value}" || "${value}" =~ ^[1-9][0-9]*$ ]] || die "invalid provider ID ${variable} in ${transition_file}"
  done
  [[ "${SOURCE_DROPLET_ID:-}" =~ ^[1-9][0-9]*$ ]] || die "transition source Droplet ID is missing"
  if [[ "${TRANSITION_KIND}" == pause ]]; then
    [[ -n "${SOURCE_DROPLET_NAME:-}" && -n "${SOURCE_REGION:-}" && -n "${SOURCE_SIZE:-}" && -n "${SNAPSHOT_NAME:-}" ]] ||
      die "pause transition is missing its immutable source or snapshot recipe"
  else
    [[ "${SNAPSHOT_ID:-}" =~ ^[1-9][0-9]*$ && -n "${PAUSE_OPERATION_ID:-}" &&
      -n "${TARGET_DROPLET_NAME:-}" && -n "${TARGET_REGION:-}" && -n "${TARGET_SIZE:-}" &&
      -n "${TARGET_LIFECYCLE_TAG:-}" && -n "${SSH_HOST_ED25519_PUBLIC_KEY:-}" ]] ||
      die "resume transition is missing its immutable snapshot or target recipe"
  fi
}

write_transition_state() {
  local variable
  atomic_write_file "${VPS_STATE_FILE}.transition" 0600 <<EOF
TRANSITION_VERSION=1
$(for variable in TRANSITION_KIND TRANSITION_PHASE OPERATION_ID PAUSE_OPERATION_ID TRANSITION_STARTED_AT \
  SOURCE_DROPLET_ID SOURCE_DROPLET_NAME SOURCE_DROPLET_IP SOURCE_REGION SOURCE_SIZE SOURCE_TAGS SOURCE_DISK_SIZE \
  REMOTE_QUIESCE_VERIFIED SHUTDOWN_REQUESTED SHUTDOWN_ACTION_ID POWER_OFF_REQUESTED POWER_OFF_ACTION_ID \
  SNAPSHOT_NAME SNAPSHOT_REQUESTED SNAPSHOT_ACTION_ID SNAPSHOT_ID SNAPSHOT_CREATED_AT SOURCE_DELETE_REQUESTED \
  SOURCE_DELETE_CONFIRMED SSH_HOST_ED25519_PUBLIC_KEY TARGET_DROPLET_NAME TARGET_REGION TARGET_SIZE TARGET_TAGS \
  TARGET_LIFECYCLE_TAG TARGET_CREATE_REQUESTED TARGET_DROPLET_ID TARGET_DROPLET_IP RECOVERY_VERIFIED_AT \
  SNAPSHOT_DELETE_REQUESTED SNAPSHOT_DELETE_CONFIRMED TEARDOWN_STARTED TARGET_DELETE_CONFIRMED; do
    eval "printf '%s=%q\\n' '${variable}' \"\${${variable}:-}\""
  done)
EOF
}

doctl_mutate() {
  doctl --http-retry-max 0 "$@"
}

wait_for_action() {
  local action_id="$1"
  local expected_type="$2"
  local expected_resource_id="$3"
  local max_attempts="${4:-180}"
  local action_json status type resource_id attempt

  [[ "${action_id}" =~ ^[1-9][0-9]*$ ]] || die "invalid action ID: ${action_id}"
  for (( attempt=1; attempt<=max_attempts; attempt++ )); do
    action_json="$(doctl compute action get "${action_id}" --output json)" || die "could not read action ${action_id}"
    [[ "$(jq 'length' <<<"${action_json}")" == 1 ]] || die "unexpected response for action ${action_id}"
    status="$(jq -er '.[0].status' <<<"${action_json}")"
    type="$(jq -er '.[0].type' <<<"${action_json}")"
    resource_id="$(jq -er '.[0].resource_id' <<<"${action_json}")"
    [[ "${type}" == "${expected_type}" && "${resource_id}" == "${expected_resource_id}" ]] ||
      die "action ${action_id} does not belong to expected ${expected_type} operation on ${expected_resource_id}"
    case "${status}" in
      completed) return 0 ;;
      errored) die "DigitalOcean action ${action_id} failed" ;;
      in-progress) ;;
      *) die "DigitalOcean action ${action_id} has unexpected status ${status}" ;;
    esac
    (( attempt < max_attempts )) || die "timed out waiting for action ${action_id}"
    sleep 10
  done
}

reconcile_action_id() {
  local expected_type="$1"
  local expected_resource_id="$2"
  local started_at="$3"
  local actions_json matches count

  actions_json="$(doctl compute action list --action-type "${expected_type}" --after "${started_at}" \
    --resource-type droplet --output json)" || return 1
  matches="$(jq --arg type "${expected_type}" --argjson resource "${expected_resource_id}" --arg started "${started_at}" \
    '[.[] | select(.type == $type and .resource_id == $resource and .started_at >= $started)]' <<<"${actions_json}")" || return 1
  count="$(jq 'length' <<<"${matches}")" || return 1
  if [[ "${count}" == 1 ]]; then
    jq -er '.[0].id' <<<"${matches}"
  elif [[ "${count}" == 0 ]]; then
    return 2
  else
    return 3
  fi
}

get_snapshot_json() {
  local snapshot_id="$1"
  local snapshot_json
  snapshot_json="$(doctl compute snapshot get "${snapshot_id}" --output json)" || return 1
  [[ "$(jq 'length' <<<"${snapshot_json}")" == 1 ]] || return 1
  printf '%s\n' "${snapshot_json}"
}

validate_snapshot_json() {
  local snapshot_json="$1"
  local expected_id="$2"
  local expected_name="$3"
  local expected_source="$4"
  local expected_region="$5"
  local target_size_disk="${6:-0}"

  jq -e --arg id "${expected_id}" --arg name "${expected_name}" --arg source "${expected_source}" \
    --arg region "${expected_region}" --argjson disk "${target_size_disk}" '
      length == 1 and .[0].id == $id and .[0].name == $name and
      .[0].resource_type == "droplet" and .[0].resource_id == $source and
      (.[0].regions | index($region) != null) and (.[0].min_disk_size <= $disk or $disk == 0)
    ' <<<"${snapshot_json}" >/dev/null
}

remote_ssh() {
  [[ -n "${DROPLET_IP:-}" ]] || die "Droplet does not have a public IP yet"
  ssh \
    -i "${SSH_PRIVATE_KEY_FILE}" \
    -o "UserKnownHostsFile=${KNOWN_HOSTS_FILE}" \
    -o "StrictHostKeyChecking=${SSH_STRICT_HOST_KEY_CHECKING:-accept-new}" \
    -o BatchMode=yes \
    -o ConnectTimeout=10 \
    -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=4 \
    "${REMOTE_SSH_USER}@${DROPLET_IP}" "$@"
}

remote_ssh_tty() {
  [[ -n "${DROPLET_IP:-}" ]] || die "Droplet does not have a public IP yet"
  ssh -tt \
    -i "${SSH_PRIVATE_KEY_FILE}" \
    -o "UserKnownHostsFile=${KNOWN_HOSTS_FILE}" \
    -o "StrictHostKeyChecking=${SSH_STRICT_HOST_KEY_CHECKING:-accept-new}" \
    -o BatchMode=yes \
    -o ConnectTimeout=10 \
    -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=4 \
    "${REMOTE_SSH_USER}@${DROPLET_IP}" "$@"
}

remote_exec() {
  local command=""
  local quoted
  local argument

  for argument in "$@"; do
    printf -v quoted '%q' "${argument}"
    command+="${command:+ }${quoted}"
  done
  remote_ssh "${command}"
}
