#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
load_state
require_command gh
require_command jq
require_command scp
require_command ssh

[[ -n "${DROPLET_IP:-}" ]] || die "Droplet does not have a public IP; destroy the provisional resource or retry allocation"
[[ -n "${REPOSITORY:-}" ]] || die "REPOSITORY is required in ${CONFIG_FILE}"
[[ -n "${BASE_BRANCH:-}" ]] || die "BASE_BRANCH is required in ${CONFIG_FILE}"
[[ -n "${WORK_BRANCH:-}" ]] || die "WORK_BRANCH is required in ${CONFIG_FILE}"
[[ "${WORK_BRANCH}" =~ ^[A-Za-z0-9._/-]+$ ]] || die "WORK_BRANCH contains unsupported characters"
[[ -n "${TASK_FILE:-}" && -s "${TASK_FILE}" ]] || die "TASK_FILE is missing or empty: ${TASK_FILE:-unset}"
[[ -n "${PR_TITLE:-}" ]] || die "PR_TITLE is required in ${CONFIG_FILE}"

CODEX_AUTH_FILE="${CODEX_AUTH_FILE:-${HOME}/.codex/auth.json}"
[[ -s "${CODEX_AUTH_FILE}" ]] || die "file-backed Codex credentials not found at ${CODEX_AUTH_FILE}; see README.md"
validate_codex_auth "${CODEX_AUTH_FILE}" || die "Codex credential file does not contain valid ChatGPT account tokens: ${CODEX_AUTH_FILE}"

if [[ -n "${GH_TOKEN:-}" ]]; then
  github_token="${GH_TOKEN}"
elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
  github_token="${GITHUB_TOKEN}"
else
  github_token="$(gh auth token)" || die "could not obtain a GitHub token; run gh auth login"
fi
[[ -n "${github_token}" ]] || die "GitHub token is empty"

job_id="job-$(date -u +%Y%m%dT%H%M%SZ)-$$"
remote_job_dir="/workspace/jobs/${job_id}"
remote_task="${remote_job_dir}/${job_id}.md"
remote_auth_upload="/home/agent/.codex/auth-upload-${job_id}.json"
remote_worker="${remote_job_dir}/vps-codex-remote-worker"
local_results_dir="$(dirname "${VPS_STATE_FILE}")/results/${job_id}"
preserve_marker="${VPS_STATE_FILE}.preserve"
install -d -m 0700 "${local_results_dir}"
[[ ! -e "${preserve_marker}" ]] || die "unresolved credential recovery marker exists at ${preserve_marker}; recover/reconcile auth before rerunning or destroying the VPS"

auth_original_sha="$(sha256_file "${CODEX_AUTH_FILE}")"
remote_cleanup_armed=false

cleanup_remote_secrets() {
  local status=$?
  if [[ "${remote_cleanup_armed}" == true ]]; then
    remote_ssh "rm -f '${remote_auth_upload}' /home/agent/.config/vps-codex/github-token" >/dev/null 2>&1 || true
  fi
  return "${status}"
}
trap cleanup_remote_secrets EXIT

send_github_token() {
  printf '%s' "${github_token}" | remote_ssh 'umask 077; cat > /home/agent/.config/vps-codex/github-token'
}

sync_codex_auth() {
  local returned_auth
  local current_sha
  local attempt

  if ! returned_auth="$(mktemp "$(dirname "${CODEX_AUTH_FILE}")/.codex-auth.remote.XXXXXX")"; then
    {
      echo "Could not create a local temporary file for potentially refreshed Codex credentials."
      echo "The VPS has intentionally been preserved. Recover /home/agent/.codex/auth.json before running 03-destroy.sh."
    } | tee "${preserve_marker}" >"${local_results_dir}/AUTH-RECOVERY.txt"
    return 1
  fi
  if ! chmod 0600 "${returned_auth}"; then
    rm -f "${returned_auth}"
    {
      echo "Could not secure a local temporary file for potentially refreshed Codex credentials."
      echo "The VPS has intentionally been preserved. Recover /home/agent/.codex/auth.json before running 03-destroy.sh."
    } | tee "${preserve_marker}" >"${local_results_dir}/AUTH-RECOVERY.txt"
    return 1
  fi

  for attempt in 1 2 3 4 5; do
    if remote_scp "${REMOTE_SSH_USER}@${DROPLET_IP}:/home/agent/.codex/auth.json" "${returned_auth}" >/dev/null 2>&1 && validate_codex_auth "${returned_auth}"; then
      current_sha="$(sha256_file "${CODEX_AUTH_FILE}")"
      if [[ "${current_sha}" == "${auth_original_sha}" ]]; then
        if mv "${returned_auth}" "${CODEX_AUTH_FILE}"; then
          remote_ssh 'rm -f /home/agent/.codex/auth.json' >/dev/null 2>&1 || true
          log "synchronized potentially refreshed Codex credentials back to ${CODEX_AUTH_FILE}"
          return 0
        fi

        printf 'Could not replace the local Codex credential. The validated remote credential remains at:\n%s\nMove it to %s before your next Codex login.\n' \
          "${returned_auth}" "${CODEX_AUTH_FILE}" >"${local_results_dir}/AUTH-RECOVERY.txt"
        log "could not replace local credentials; validated remote credentials remain at ${returned_auth}"
        return 2
      fi

      printf 'Local Codex credentials changed concurrently. The remote credential was safely retrieved to:\n%s\nReconcile these files before your next Codex login.\n' "${returned_auth}" >"${local_results_dir}/AUTH-RECOVERY.txt"
      log "local credentials changed during the job; remote credentials are preserved at ${returned_auth}"
      return 2
    fi
    sleep $((attempt * 2))
  done

  rm -f "${returned_auth}"
  {
    echo "Codex may have rotated its ChatGPT-account refresh token, but the updated auth file could not be retrieved."
    echo "The VPS has intentionally been preserved. Recover /home/agent/.codex/auth.json before running 03-destroy.sh."
  } | tee "${preserve_marker}" >"${local_results_dir}/AUTH-RECOVERY.txt"
  return 1
}

worker_args=(
  "${REPOSITORY}"
  "${BASE_BRANCH}"
  "${WORK_BRANCH}"
  "${remote_task}"
  "${PR_TITLE}"
  "${GIT_AUTHOR_NAME:-Codex VPS Agent}"
  "${GIT_AUTHOR_EMAIL:-codex-vps-agent@users.noreply.github.com}"
)

log "preparing remote job ${job_id}"
remote_ssh "install -d -m 0755 '${remote_job_dir}'; install -d -m 0700 /home/agent/.codex /home/agent/.config/vps-codex"
remote_cleanup_armed=true

remote_scp "${CODEX_AUTH_FILE}" "${REMOTE_SSH_USER}@${DROPLET_IP}:${remote_auth_upload}"
remote_ssh "install -m 0600 '${remote_auth_upload}' /home/agent/.codex/auth.json && rm -f '${remote_auth_upload}'"
remote_scp "${TASK_FILE}" "${REMOTE_SSH_USER}@${DROPLET_IP}:${remote_task}"
remote_scp "${SCRIPT_DIR}/remote-worker.sh" "${REMOTE_SSH_USER}@${DROPLET_IP}:${remote_worker}"
remote_ssh "chmod 0755 '${remote_worker}'; chmod 0644 '${remote_task}'"

# GitHub credentials exist only during clone and publication, never while
# Codex or repository-controlled test/build commands execute.
send_github_token
remote_exec "${remote_worker}" prepare "${worker_args[@]}"

printf 'Starting Codex on %s. Do not run another Codex process using the same auth file until this phase finishes.\n' "${DROPLET_IP}" >&2
set +e
remote_exec "${remote_worker}" run "${worker_args[@]}" \
  2>&1 | tee "${local_results_dir}/job.log"
job_status=${PIPESTATUS[0]}
set -e

set +e
sync_codex_auth
auth_sync_status=$?
set -e
if (( auth_sync_status == 1 )); then
  die "could not recover potentially refreshed Codex credentials; VPS preserved for manual recovery (see ${local_results_dir}/AUTH-RECOVERY.txt)"
elif (( auth_sync_status == 2 )); then
  die "Codex credentials changed concurrently; reconcile the safely retrieved file described in ${local_results_dir}/AUTH-RECOVERY.txt"
fi

if (( job_status != 0 )); then
  die "remote Codex job failed with status ${job_status}; inspect ${local_results_dir}/job.log"
fi

send_github_token
remote_exec "${remote_worker}" publish "${worker_args[@]}"
unset github_token

remote_scp "${REMOTE_SSH_USER}@${DROPLET_IP}:${remote_job_dir}/result.json" "${local_results_dir}/result.json"
remote_scp "${REMOTE_SSH_USER}@${DROPLET_IP}:${remote_job_dir}/codex-final.md" "${local_results_dir}/codex-final.md"

pr_url="$(jq -er '.pullRequest' "${local_results_dir}/result.json")"
printf '\nCodex job succeeded\n  results: %s\n  draft PR: %s\n' "${local_results_dir}" "${pr_url}"
