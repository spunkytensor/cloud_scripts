#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

instance_id=""
create_new_instance=false
instance_option_seen=false
while (( $# > 0 )); do
  case "$1" in
    --new)
      [[ "${create_new_instance}" != true ]] || die "--new may only be specified once"
      create_new_instance=true
      shift
      ;;
    --instance)
      (( $# >= 2 )) || die "--instance requires an instance ID"
      [[ "${instance_option_seen}" != true ]] || die "--instance may only be specified once"
      [[ -n "$2" ]] || die "--instance requires a nonempty instance ID"
      instance_option_seen=true
      instance_id="$2"
      shift 2
      ;;
    *)
      die "usage: ./create.sh [--new | --instance INSTANCE_ID]"
      ;;
  esac
done
[[ "${create_new_instance}" != true || "${instance_option_seen}" != true ]] || die "--new and --instance cannot be combined"
[[ "${create_new_instance}" == true || "${instance_option_seen}" == true ]] || die "usage: ./create.sh (--new | --instance INSTANCE_ID)"

load_config
if [[ "${create_new_instance}" == true ]]; then
  install -d -m 0700 "$(instance_state_root)" || die "cannot create instance state directory: $(instance_state_root)"
  instance_id=""
  for attempt in {1..20}; do
    candidate_instance_id="worker-$(date -u +%Y%m%d-%H%M%S)-$$-${RANDOM}"
    if mkdir -m 0700 "$(instance_state_root)/${candidate_instance_id}" 2>/dev/null; then
      instance_id="${candidate_instance_id}"
      break
    fi
  done
  [[ -n "${instance_id}" ]] || die "could not reserve a unique instance ID after 20 attempts"
  instance_option_seen=true
fi
select_instance_state "${instance_id}"
instance_arguments=" --instance ${instance_id}"
log "using instance ${instance_id}"
create_command="./create.sh${instance_arguments}"
destroy_command="./destroy.sh${instance_arguments}"

require_command doctl
require_command git
require_command gh
require_command jq
require_command ssh
require_command ssh-add
require_command ssh-keygen

[[ -n "${DO_SSH_KEY:-}" ]] || die "DO_SSH_KEY is required in ${CONFIG_FILE}"
[[ -n "${SSH_PRIVATE_KEY_FILE:-}" ]] || die "SSH_PRIVATE_KEY_FILE is required in ${CONFIG_FILE}"
[[ -f "${SSH_PRIVATE_KEY_FILE}" ]] || die "SSH private key not found: ${SSH_PRIVATE_KEY_FILE}"
[[ -f "${SSH_PRIVATE_KEY_FILE}.pub" ]] || die "SSH public key not found: ${SSH_PRIVATE_KEY_FILE}.pub"
[[ -n "${REPOSITORY:-}" ]] || die "REPOSITORY is required in ${CONFIG_FILE}"
[[ -n "${BASE_BRANCH:-}" ]] || die "BASE_BRANCH is required in ${CONFIG_FILE}"
WORK_BRANCH="codex/${instance_id}"
[[ "${BASE_BRANCH}" =~ ^[A-Za-z0-9._/-]+$ ]] || die "BASE_BRANCH contains unsupported characters"
[[ "${WORK_BRANCH}" =~ ^[A-Za-z0-9._/-]+$ ]] || die "WORK_BRANCH contains unsupported characters"
git check-ref-format --branch "${WORK_BRANCH}" >/dev/null 2>&1 || die "WORK_BRANCH is not a valid Git branch: ${WORK_BRANCH}"

repository="${REPOSITORY#https://github.com/}"
repository="${repository#git@github.com:}"
repository="${repository%.git}"
repository="${repository%/}"
[[ "${repository}" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "REPOSITORY must identify one github.com owner/repository"

state_dir="$(dirname "${VPS_STATE_FILE}")"
cloud_init_file=""

cleanup() {
  local status=$?
  [[ -z "${cloud_init_file}" ]] || rm -f "${cloud_init_file}"
  release_lifecycle_lock
  if (( status != 0 )) && [[ -e "${VPS_STATE_FILE}" || -e "${VPS_STATE_FILE}.allocation" ]]; then
    printf '\nSetup stopped. The Droplet remains allocated and may be billable.\nResume with:\n  %s\n\nAbandon and clean up with:\n  %s\n' "${create_command}" "${destroy_command}" >&2
  fi
  return "${status}"
}
trap cleanup EXIT

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

install -d -m 0700 "${state_dir}" || die "cannot create state directory: ${state_dir}"
state_probe="$(mktemp "${VPS_STATE_FILE}.probe.XXXXXX")" || die "state directory is not writable: ${state_dir}"
rm -f "${state_probe}"
acquire_lifecycle_lock

if [[ -e "${VPS_STATE_FILE}" ]]; then
  load_state
  droplet_id="${DROPLET_ID}"
  droplet_name="${DROPLET_NAME:-unknown}"
  log "resuming setup for Droplet ${droplet_id} from ${VPS_STATE_FILE}"
else
  for orphaned_state in \
    "${VPS_STATE_FILE}.github-token" \
    "${VPS_STATE_FILE}.github-token.replacement" \
    "${VPS_STATE_FILE}.setup" \
    "${VPS_STATE_FILE}.remote-control"; do
    [[ ! -e "${orphaned_state}" ]] || die "orphaned lifecycle state exists at ${orphaned_state}; run ${destroy_command} before creating another Droplet"
  done

  cloud_init_file="$(mktemp "${state_dir}/cloud-init.XXXXXX.yaml")" || die "cannot create rendered cloud-init file"
  chmod 0600 "${cloud_init_file}"
  sed "s|__AGENT_SSH_AUTHORIZED_KEY__|${registered_public_key}|" "${SCRIPT_DIR}/cloud-init.yaml" >"${cloud_init_file}"

  allocation_state_file="${VPS_STATE_FILE}.allocation"
  if [[ -f "${allocation_state_file}" ]]; then
    # Generated below with shell-escaped values.
    # shellcheck disable=SC1090
    source "${allocation_state_file}"
  else
    ALLOCATION_DROPLET_NAME="${DROPLET_NAME_PREFIX:-codex-agent}-${VPS_INSTANCE_ID}"
    ALLOCATION_REGION="${DO_REGION:-nyc3}"
    ALLOCATION_SIZE="${DO_SIZE:-s-4vcpu-8gb}"
    ALLOCATION_IMAGE="${DO_IMAGE:-ubuntu-24-04-x64}"
    ALLOCATION_TAGS="${DO_TAGS:-codex-agent}"
    ALLOCATION_LIFECYCLE_TAG="codex-lifecycle-$(date -u +%Y%m%d%H%M%S)-$$-${RANDOM}"
    ALLOCATION_REQUESTED=0
  fi

  write_allocation_state() {
    local temporary_allocation
    temporary_allocation="$(mktemp "${allocation_state_file}.tmp.XXXXXX")"
    chmod 0600 "${temporary_allocation}"
    {
      printf 'ALLOCATION_DROPLET_NAME=%q\n' "${ALLOCATION_DROPLET_NAME}"
      printf 'ALLOCATION_REGION=%q\n' "${ALLOCATION_REGION}"
      printf 'ALLOCATION_SIZE=%q\n' "${ALLOCATION_SIZE}"
      printf 'ALLOCATION_IMAGE=%q\n' "${ALLOCATION_IMAGE}"
      printf 'ALLOCATION_TAGS=%q\n' "${ALLOCATION_TAGS}"
      printf 'ALLOCATION_LIFECYCLE_TAG=%q\n' "${ALLOCATION_LIFECYCLE_TAG}"
      printf 'ALLOCATION_REQUESTED=%q\n' "${ALLOCATION_REQUESTED}"
    } >"${temporary_allocation}"
    mv "${temporary_allocation}" "${allocation_state_file}"
  }

  [[ -n "${ALLOCATION_LIFECYCLE_TAG:-}" ]] || die "invalid allocation checkpoint: ${allocation_state_file}"
  if [[ "${ALLOCATION_REQUESTED:-0}" == 1 ]]; then
    log "reconciling an earlier DigitalOcean allocation request"
    allocation_matches="$(doctl compute droplet list --tag-name "${ALLOCATION_LIFECYCLE_TAG}" --output json)"
    allocation_count="$(jq 'length' <<<"${allocation_matches}")"
    if [[ "${allocation_count}" == 1 ]]; then
      droplet_id="$(jq -er '.[0].id' <<<"${allocation_matches}")"
      droplet_name="$(jq -er '.[0].name' <<<"${allocation_matches}")"
      write_state "${droplet_id}" "" "${droplet_name}" || die "could not persist the reconciled Droplet state"
      rm -f "${allocation_state_file}"
      log "recovered Droplet ${droplet_id} from the earlier allocation request"
    elif [[ "${allocation_count}" == 0 ]]; then
      die "the earlier allocation request has no visible Droplet yet; retry later, or run ${destroy_command} to clear the checkpoint after confirming no Droplet exists"
    else
      die "multiple Droplets have lifecycle tag ${ALLOCATION_LIFECYCLE_TAG}; delete the extras manually before continuing"
    fi
  else
    write_allocation_state
    ALLOCATION_REQUESTED=1
    write_allocation_state
    log "requesting ${ALLOCATION_DROPLET_NAME} (${ALLOCATION_SIZE}, ${ALLOCATION_REGION}, ${ALLOCATION_IMAGE}) from DigitalOcean"
    set +e
    response="$(doctl compute droplet create "${ALLOCATION_DROPLET_NAME}" \
      --region "${ALLOCATION_REGION}" \
      --size "${ALLOCATION_SIZE}" \
      --image "${ALLOCATION_IMAGE}" \
      --ssh-keys "${DO_SSH_KEY}" \
      --tag-names "${ALLOCATION_TAGS},${ALLOCATION_LIFECYCLE_TAG}" \
      --enable-monitoring \
      --user-data-file "${cloud_init_file}" \
      --output json 2>&1)"
    create_status=$?
    set -e
    if (( create_status != 0 )); then
      [[ -z "${response}" ]] || printf '%s\n' "${response}" >&2
      http_status="$(doctl_http_status <<<"${response}")"
      if [[ "${http_status}" =~ ^4[0-9]{2}$ && "${http_status}" != 408 && "${http_status}" != 429 ]]; then
        rm -f "${allocation_state_file}"
        die "DigitalOcean rejected the allocation request; no request checkpoint was retained, so fix the reported error and rerun ${create_command}"
      fi
      die "the allocation result is uncertain; rerun ${create_command} to reconcile by lifecycle tag or use ${destroy_command} after confirming no resource was created"
    fi

    if ! droplet_id="$(jq -er '.[0].id' <<<"${response}")"; then
      die "DigitalOcean returned an unrecognized create response; rerun ${create_command} to reconcile by lifecycle tag"
    fi
    droplet_name="${ALLOCATION_DROPLET_NAME}"
    if ! write_state "${droplet_id}" "" "${droplet_name}"; then
      printf '\nCRITICAL: DigitalOcean created Droplet %s (%s), but its local state could not be saved.\n' \
        "${droplet_id}" "${droplet_name}" >&2
      log "attempting immediate rollback of untracked Droplet ${droplet_id}"
      set +e
      rollback_output="$(doctl compute droplet delete "${droplet_id}" --force 2>&1)"
      rollback_status=$?
      set -e
      if (( rollback_status == 0 )); then
        rm -f "${allocation_state_file}"
        die "rolled back Droplet ${droplet_id} after the state write failed"
      fi
      [[ -z "${rollback_output}" ]] || printf '%s\n' "${rollback_output}" >&2
      die "rollback failed: Droplet ${droplet_id} (${droplet_name}) may still be running and billable; delete that ID manually or run ${destroy_command} to reconcile its lifecycle tag"
    fi
    rm -f "${allocation_state_file}"
    log "DigitalOcean accepted the request as Droplet ${droplet_id}; cleanup state saved to ${VPS_STATE_FILE}"
  fi
fi

setup_state_file="${VPS_STATE_FILE}.setup"
if [[ -f "${setup_state_file}" ]]; then
  unset SETUP_REPOSITORY SETUP_BASE_BRANCH SETUP_WORK_BRANCH SETUP_INSTANCE_ID SETUP_CREDENTIAL_ISOLATION_VERSION
  # Generated below with shell-escaped values.
  # shellcheck disable=SC1090
  source "${setup_state_file}"
  [[ "${SETUP_REPOSITORY:-}" == "${repository}" ]] || die "configured REPOSITORY differs from the environment recorded in ${setup_state_file}"
  [[ "${SETUP_BASE_BRANCH:-}" == "${BASE_BRANCH}" ]] || die "configured BASE_BRANCH differs from the environment recorded in ${setup_state_file}"
  [[ -n "${SETUP_WORK_BRANCH:-}" ]] || die "invalid setup state: ${setup_state_file}"
  [[ "${SETUP_CREDENTIAL_ISOLATION_VERSION:-}" == 1 && "${SETUP_INSTANCE_ID:-}" == "${instance_id}" ]] ||
    die "instance setup state predates credential-isolation ownership markers; refusing to reuse its branch or credential"
  [[ "${SETUP_WORK_BRANCH}" == "${WORK_BRANCH}" ]] ||
    die "instance branch (${SETUP_WORK_BRANCH}) does not match the isolated branch required for ${instance_id} (${WORK_BRANCH})"
  WORK_BRANCH="${SETUP_WORK_BRANCH}"
else
  temporary_setup="$(mktemp "${setup_state_file}.tmp.XXXXXX")"
  chmod 0600 "${temporary_setup}"
  {
    printf 'SETUP_REPOSITORY=%q\n' "${repository}"
    printf 'SETUP_BASE_BRANCH=%q\n' "${BASE_BRANCH}"
    printf 'SETUP_WORK_BRANCH=%q\n' "${WORK_BRANCH}"
    printf 'SETUP_INSTANCE_ID=%q\n' "${instance_id}"
    printf 'SETUP_CREDENTIAL_ISOLATION_VERSION=1\n'
  } >"${temporary_setup}"
  mv "${temporary_setup}" "${setup_state_file}"
fi

log "waiting for DigitalOcean to report Droplet ${droplet_id} and its public IP"
droplet_ip=""
last_ip_query_error=""
for attempt in {1..60}; do
  set +e
  droplet_json="$(doctl compute droplet get "${droplet_id}" --output json 2>&1)"
  ip_query_status=$?
  set -e
  if (( ip_query_status == 0 )); then
    last_ip_query_error=""
    droplet_ip="$(jq -r '.[0].networks.v4[]? | select(.type == "public") | .ip_address' <<<"${droplet_json}" | head -n 1)"
  else
    last_ip_query_error="${droplet_json}"
    [[ -z "${droplet_json}" ]] || printf '%s\n' "${droplet_json}" >&2
    http_status="$(doctl_http_status <<<"${droplet_json}")"
    if [[ "${http_status}" =~ ^4[0-9]{2}$ && "${http_status}" != 408 && "${http_status}" != 429 ]]; then
      die "DigitalOcean rejected the lookup for Droplet ${droplet_id}; fix the non-transient API error shown above or run ${destroy_command}"
    fi
    log "DigitalOcean IP lookup failed transiently (attempt ${attempt}/60); retrying"
  fi
  if [[ -n "${droplet_ip}" ]]; then
    break
  fi
  if (( attempt == 60 )); then
    if [[ -n "${last_ip_query_error}" ]]; then
      die "DigitalOcean repeatedly failed to query Droplet ${droplet_id}; last error is shown above"
    fi
    die "DigitalOcean reported Droplet ${droplet_id}, but did not assign it a public IP"
  fi
  sleep 5
done

write_state "${droplet_id}" "${droplet_ip}" "${droplet_name}"
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
    die "SSH for ${REMOTE_SSH_USER} did not become ready"
  fi
  sleep 5
done

log "waiting for cloud-init (this installs Docker, Node.js, GitHub CLI, and Codex)"
remote_ssh 'cloud-init status --wait >/dev/null && test -f /opt/codex-worker-ready'
remote_ssh 'id -nG | tr " " "\n" | grep -Fxq docker && docker info >/dev/null' ||
  die "agent does not have working Docker access; this Droplet may predate Docker-enabled provisioning. Recreate it with ${destroy_command} followed by ${create_command}, or use the DigitalOcean root console to run: usermod -aG docker agent"

token_state_file="${VPS_STATE_FILE}.github-token"
replacement_journal_file="${token_state_file}.replacement"
if [[ -e "${token_state_file}" && ! -s "${token_state_file}" ]]; then
  rm -f "${token_state_file}"
  log "removed an empty GitHub credential marker left by an interrupted write"
fi

if ! remote_github_state="$(remote_ssh '
  if test -s /home/agent/.config/vps-codex/github-token; then
    printf present
  elif test -e /home/agent/.config/vps-codex/github-token; then
    rm -f /home/agent/.config/vps-codex/github-token
    printf absent
  else
    printf absent
  fi
')"; then
  die "could not inspect the VPS GitHub credential state"
fi

token_url="https://github.com/settings/personal-access-tokens/new?name=$(printf '%s' "Codex VPS ${DROPLET_ID}" | jq -sRr @uri)&description=$(printf '%s' "Temporary autonomous worker for ${repository}" | jq -sRr @uri)&target_name=$(printf '%s' "${repository%%/*}" | jq -sRr @uri)&expires_in=2&contents=write&pull_requests=write&actions=read&statuses=read"

read_new_github_token() {
  printf '\nCreate a dedicated fine-grained personal access token for this VPS:\n\n  %s\n\nSelect only repository %s and confirm the requested permissions.\nThe token should expire in two days. Paste it below; input will not be echoed.\n\n' \
    "${token_url}" "${repository}" >&2
  [[ -t 0 ]] || die "standard input is not a terminal; GitHub token provisioning requires an interactive prompt"
  IFS= read -r -s -p "VPS-specific fine-grained PAT: " new_github_token
  printf '\n' >&2
  [[ "${new_github_token}" == github_pat_* ]] || die "expected a fine-grained PAT beginning with github_pat_; refusing a broader or unknown credential type"
}

save_github_token() {
  temporary_token="$(mktemp "${state_dir}/github-token.tmp.XXXXXX")" || return 1
  chmod 0600 "${temporary_token}" || { rm -f "${temporary_token}"; return 1; }
  printf '%s' "${github_token}" >"${temporary_token}" || { rm -f "${temporary_token}"; return 1; }
  mv "${temporary_token}" "${token_state_file}" || { rm -f "${temporary_token}"; return 1; }
}

write_replacement_journal() {
  local old_token="$1"
  local new_token="$2"
  local temporary_journal

  temporary_journal="$(mktemp "${replacement_journal_file}.tmp.XXXXXX")" || return 1
  chmod 0600 "${temporary_journal}" || { rm -f "${temporary_journal}"; return 1; }
  {
    printf 'REPLACEMENT_OLD_GITHUB_TOKEN=%q\n' "${old_token}"
    printf 'REPLACEMENT_NEW_GITHUB_TOKEN=%q\n' "${new_token}"
  } >"${temporary_journal}" || { rm -f "${temporary_journal}"; return 1; }
  mv "${temporary_journal}" "${replacement_journal_file}" || { rm -f "${temporary_journal}"; return 1; }
}

validate_github_token() {
  local candidate_token="$1"
  local candidate_repo_json

  candidate_repo_json="$(GH_TOKEN="${candidate_token}" gh api "repos/${repository}" 2>/dev/null)" || return 1
  [[ "$(jq -r '.full_name' <<<"${candidate_repo_json}")" == "${repository}" ]] || return 1
  github_actor="$(GH_TOKEN="${candidate_token}" gh api user --jq .login 2>/dev/null)" || return 1
}

install_remote_github_token() {
  local replacement_allowed="$1"
  local token_sync_status

  set +e
  printf '%s' "${github_token}" | remote_exec bash -c '
  set -euo pipefail
  replacement_allowed="$1"
  token_file=/home/agent/.config/vps-codex/github-token
  temporary_token="$(mktemp /home/agent/.config/vps-codex/github-token.verify.XXXXXX)"
  trap '\''rm -f "${temporary_token}"'\'' EXIT
  chmod 0600 "${temporary_token}"
  cat >"${temporary_token}"
  if test "${replacement_allowed}" != true && test -s "${token_file}" && ! cmp -s "${temporary_token}" "${token_file}"; then
    exit 42
  fi
  install -m 0600 "${temporary_token}" "${token_file}"
' -- "${replacement_allowed}"
  token_sync_status=$?
  set -e
  if (( token_sync_status == 42 )); then
    log "the host and VPS GitHub credentials differ; refusing to overwrite either one"
    return 42
  elif (( token_sync_status != 0 )); then
    log "could not install or verify the GitHub credential on the VPS"
    return "${token_sync_status}"
  fi
}

classify_remote_replacement_token() {
  local old_token="$1"
  local new_token="$2"

  printf '%s\n%s\n' "${old_token}" "${new_token}" | remote_exec bash -c '
    set -euo pipefail
    token_file=/home/agent/.config/vps-codex/github-token
    old_file="$(mktemp /home/agent/.config/vps-codex/github-token.old.XXXXXX)"
    new_file="$(mktemp /home/agent/.config/vps-codex/github-token.new.XXXXXX)"
    trap '\''rm -f "${old_file}" "${new_file}"'\'' EXIT
    chmod 0600 "${old_file}" "${new_file}"
    IFS= read -r old_token
    IFS= read -r new_token
    printf %s "${old_token}" >"${old_file}"
    printf %s "${new_token}" >"${new_file}"
    if test ! -s "${token_file}"; then
      printf absent
    elif cmp -s "${token_file}" "${old_file}"; then
      printf old
    elif cmp -s "${token_file}" "${new_file}"; then
      printf new
    else
      printf other
    fi
  '
}

revoke_superseded_token() {
  local old_token="$1"
  local response_file

  if ! command -v curl >/dev/null 2>&1; then
    log "WARNING: curl is unavailable; the superseded token could not be revoked and must be revoked manually or allowed to expire"
    return 1
  fi
  response_file="$(mktemp "${state_dir}/github-revoke.XXXXXX")" || return 1
  chmod 0600 "${response_file}" || { rm -f "${response_file}"; return 1; }
  if printf '%s' "${old_token}" | jq -Rsc '{credentials: [.]}' | \
    curl --fail-with-body --silent --show-error \
      --request POST \
      --header 'Accept: application/vnd.github+json' \
      --header 'Content-Type: application/json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --data-binary @- \
      --output "${response_file}" \
      https://api.github.com/credentials/revoke; then
    rm -f "${response_file}"
    log "revoked the superseded VPS-specific GitHub credential"
    return 0
  fi
  rm -f "${response_file}"
  log "WARNING: the superseded token could not be revoked; revoke it manually or allow its two-day lifetime to expire"
  return 1
}

if [[ -f "${replacement_journal_file}" ]]; then
  # Generated by write_replacement_journal with shell-escaped values.
  # shellcheck disable=SC1090
  source "${replacement_journal_file}"
  [[ "${REPLACEMENT_OLD_GITHUB_TOKEN:-}" == github_pat_* ]] || die "invalid old credential in replacement journal ${replacement_journal_file}"
  [[ "${REPLACEMENT_NEW_GITHUB_TOKEN:-}" == github_pat_* ]] || die "invalid new credential in replacement journal ${replacement_journal_file}"
  [[ -s "${token_state_file}" ]] || die "replacement journal exists without a retained host credential: ${replacement_journal_file}"
  retained_host_token="$(cat "${token_state_file}")"
  if [[ "${retained_host_token}" != "${REPLACEMENT_OLD_GITHUB_TOKEN}" &&
    "${retained_host_token}" != "${REPLACEMENT_NEW_GITHUB_TOKEN}" ]]; then
    die "retained host credential is not one of the two values recorded in ${replacement_journal_file}"
  fi
  remote_replacement_state="$(classify_remote_replacement_token "${REPLACEMENT_OLD_GITHUB_TOKEN}" "${REPLACEMENT_NEW_GITHUB_TOKEN}")" ||
    die "could not inspect the interrupted GitHub credential replacement"
  [[ "${remote_replacement_state}" == old || "${remote_replacement_state}" == new ]] ||
    die "remote credential is not one of the two values recorded in ${replacement_journal_file}; refusing to overwrite it"
  validate_github_token "${REPLACEMENT_NEW_GITHUB_TOKEN}" || die "the pending replacement token is no longer authorized for ${repository}; journal retained for secure teardown"
  github_token="${REPLACEMENT_NEW_GITHUB_TOKEN}"
  install_remote_github_token true || die "could not resume the interrupted remote credential replacement"
  save_github_token || die "could not finish saving the replacement host credential; replacement journal retained for resume"
  if revoke_superseded_token "${REPLACEMENT_OLD_GITHUB_TOKEN}"; then
    rm -f "${replacement_journal_file}"
  else
    log "retaining the replacement journal so destroy.sh can retry revocation of both known credentials"
  fi
  unset retained_host_token remote_replacement_state github_token
  unset REPLACEMENT_OLD_GITHUB_TOKEN REPLACEMENT_NEW_GITHUB_TOKEN
  log "reconciled the interrupted GitHub credential replacement"
elif [[ -s "${token_state_file}" ]]; then
  github_token="$(cat "${token_state_file}")"
  [[ "${github_token}" == github_pat_* ]] || die "retained GitHub credential has an unexpected type"
  log "verifying the retained VPS-specific GitHub credential and its remote copy"
  install_remote_github_token false || die "could not safely reconcile the retained GitHub credential"
  if ! validate_github_token "${github_token}"; then
    old_github_token="${github_token}"
    log "the retained GitHub credential is expired, revoked, or no longer authorized; requesting a replacement without changing the VPS checkout"
    read_new_github_token
    [[ "${new_github_token}" != "${old_github_token}" ]] || die "the replacement token is identical to the rejected token; create a new token"
    validate_github_token "${new_github_token}" || die "the replacement token cannot access ${repository}; verify its repository selection, approval, and expiration"
    write_replacement_journal "${old_github_token}" "${new_github_token}" || die "could not create the protected credential replacement journal; no credential was changed"
    github_token="${new_github_token}"
    install_remote_github_token true || die "could not install the replacement credential; replacement journal retained for safe resume or teardown"
    save_github_token || die "could not save the replacement host credential; replacement journal retained for safe resume or teardown"
    if revoke_superseded_token "${old_github_token}"; then
      rm -f "${replacement_journal_file}"
    else
      log "retaining the replacement journal so destroy.sh can retry revocation of both known credentials"
    fi
    unset old_github_token new_github_token
    log "installed the replacement GitHub credential; the existing checkout and work branch were preserved"
  fi
else
  [[ "${remote_github_state}" == absent ]] || die "the VPS has an untracked GitHub credential; remove or recover it before resuming"
  read_new_github_token
  validate_github_token "${new_github_token}" || die "the token cannot access ${repository}; verify its repository selection, approval, and expiration"
  github_token="${new_github_token}"
  save_github_token
  log "installing the repository-scoped GitHub credential on the VPS"
  install_remote_github_token false || die "could not install the GitHub credential on the VPS; the protected host copy was retained for resume"
  unset new_github_token
fi
unset github_token

log "preparing ${repository} on ${WORK_BRANCH}"
remote_exec bash -s -- \
  "${repository}" \
  "${BASE_BRANCH}" \
  "${WORK_BRANCH}" \
  "${GIT_AUTHOR_NAME:-Codex VPS Agent}" \
  "${GIT_AUTHOR_EMAIL:-codex-vps-agent@users.noreply.github.com}" <<'REMOTE'
set -euo pipefail

repository="$1"
base_branch="$2"
work_branch="$3"
git_name="$4"
git_email="$5"
projects_dir="${HOME}/projects"
checkout_dir="${projects_dir}/${repository##*/}"
expected_origin="https://github.com/${repository}.git"

[[ "$(id -un)" == agent ]] || { echo "workspace provisioning must run as agent" >&2; exit 1; }
[[ -s "${HOME}/.config/vps-codex/github-token" ]] || { echo "GitHub credential is missing" >&2; exit 1; }
install -d -m 0755 "${projects_dir}"

git config --global --unset-all 'credential.https://github.com.helper' >/dev/null 2>&1 || true
git config --global --add 'credential.https://github.com.helper' ''
git config --global --add 'credential.https://github.com.helper' '!/usr/local/bin/gh auth git-credential'

if [[ ! -e "${checkout_dir}" ]]; then
  git clone --branch "${base_branch}" --single-branch "${expected_origin}" "${checkout_dir}"
elif [[ ! -d "${checkout_dir}/.git" ]]; then
  echo "checkout path exists but is not a Git repository: ${checkout_dir}" >&2
  exit 1
fi

cd "${checkout_dir}"
[[ "$(git remote get-url origin)" == "${expected_origin}" ]] || { echo "checkout origin does not match ${expected_origin}" >&2; exit 1; }
git config --local user.name "${git_name}"
git config --local user.email "${git_email}"
if ! git show-ref --verify --quiet "refs/heads/${work_branch}"; then
  [[ -z "$(git status --porcelain)" ]] || { echo "cannot create ${work_branch}; checkout has uncommitted changes" >&2; exit 1; }
  git switch -c "${work_branch}"
fi
[[ "$(git symbolic-ref --short HEAD 2>/dev/null || true)" == "${work_branch}" ]] || {
  echo "checkout is not on ${work_branch}; switch branches or use a separate environment before resuming" >&2
  exit 1
}
git ls-remote --exit-code origin "refs/heads/${base_branch}" >/dev/null
/usr/local/bin/gh repo view "${repository}" --json nameWithOwner --jq .nameWithOwner >/dev/null
REMOTE

checkout_dir="/home/agent/projects/${repository##*/}"
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
  [[ -f "${remote_control_marker}" ]] || die "the VPS has an untracked ChatGPT credential; verify its owner, then run 'codex logout' on the VPS before resuming"
  # shellcheck disable=SC1090
  source "${remote_control_marker}"
  enrolled_at="${REMOTE_CONTROL_ENROLLED_AT:-${REMOTE_CONTROL_STARTED_AT:-}}"
  [[ -n "${enrolled_at}" ]] || die "remote-control enrollment marker is invalid: ${remote_control_marker}"
elif [[ "${login_status}" == *"Not logged in"* ]]; then
  log "starting the headless ChatGPT device-login flow"
  remote_ssh_tty "${remote_codex} login --device-auth"
  enrolled_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  write_remote_control_marker
else
  die "could not determine remote Codex login status: ${login_status}"
fi

login_status="$(remote_ssh "${remote_codex} login status" 2>&1)"
[[ "${login_status}" == *"Logged in using ChatGPT"* ]] || die "ChatGPT login did not complete: ${login_status}"

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

printf '\nCodex VPS ready\n  Instance: %s\n  Droplet: %s\n  IP: %s\n  SSH host: %s\n  checkout: %s\n  branch: %s\n  GitHub actor: %s\n  remote-control server: %s\n  pairing code: %s\n  code expires at: %s (Unix time)\n\nConnect with:\n  ssh -F %q %q\n\nApprove the pairing code from the Codex/ChatGPT client.\nThe daemon survives SSH logout but not a VPS reboot; rerun %s to restart and pair it.\n\nDestroy this billable environment with:\n  %s\n' \
  "${VPS_INSTANCE_ID}" "${DROPLET_ID}" "${DROPLET_IP}" "${SSH_ALIAS}" "${checkout_dir}" "${WORK_BRANCH}" "${github_actor}" \
  "${server_name}" "${pairing_code}" "${expires_at}" "${SSH_CONFIG_FILE}" "${SSH_ALIAS}" "${create_command}" "${destroy_command}"
