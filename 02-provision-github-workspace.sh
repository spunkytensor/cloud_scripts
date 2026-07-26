#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

load_config
load_state
require_command gh
require_command jq
require_command ssh

[[ -n "${DROPLET_IP:-}" ]] || die "Droplet does not have a public IP"
[[ -n "${REPOSITORY:-}" ]] || die "REPOSITORY is required in ${CONFIG_FILE}"
[[ -n "${BASE_BRANCH:-}" ]] || die "BASE_BRANCH is required in ${CONFIG_FILE}"
[[ -n "${WORK_BRANCH:-}" ]] || die "WORK_BRANCH is required in ${CONFIG_FILE}"
[[ "${BASE_BRANCH}" =~ ^[A-Za-z0-9._/-]+$ ]] || die "BASE_BRANCH contains unsupported characters"
[[ "${WORK_BRANCH}" =~ ^[A-Za-z0-9._/-]+$ ]] || die "WORK_BRANCH contains unsupported characters"

repository="${REPOSITORY#https://github.com/}"
repository="${repository#git@github.com:}"
repository="${repository%.git}"
repository="${repository%/}"
[[ "${repository}" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "REPOSITORY must identify one github.com owner/repository"

token_state_file="${VPS_STATE_FILE}.github-token"
[[ ! -e "${token_state_file}" ]] || die "a persistent GitHub credential already exists at ${token_state_file}; destroy this VPS before provisioning another"
if ! remote_github_state="$(remote_ssh '
  if test -e /home/agent/.config/vps-codex/github-token; then
    printf present
  else
    printf absent
  fi
')"; then
  die "could not verify whether the VPS already contains a persistent GitHub credential"
fi
[[ "${remote_github_state}" == absent ]] || die "the VPS already contains an untracked persistent GitHub credential"

token_url="https://github.com/settings/personal-access-tokens/new?name=$(printf '%s' "Codex VPS ${DROPLET_ID}" | jq -sRr @uri)&description=$(printf '%s' "Temporary autonomous worker for ${repository}" | jq -sRr @uri)&target_name=$(printf '%s' "${repository%%/*}" | jq -sRr @uri)&expires_in=2&contents=write&pull_requests=write&actions=read&statuses=read"

if [[ -n "${GITHUB_TOKEN_FILE:-}" ]]; then
  [[ -f "${GITHUB_TOKEN_FILE}" ]] || die "GITHUB_TOKEN_FILE does not exist: ${GITHUB_TOKEN_FILE}"
  github_token="$(tr -d '\r\n' <"${GITHUB_TOKEN_FILE}")"
else
  printf '\nCreate a dedicated fine-grained personal access token for this VPS:\n\n  %s\n\nSelect only repository %s and confirm the requested permissions.\nThe token should expire in two days. Paste it below; input will not be echoed.\n\n' \
    "${token_url}" "${repository}" >&2
  [[ -t 0 ]] || die "standard input is not a terminal; set GITHUB_TOKEN_FILE to a protected file containing the VPS-specific token"
  IFS= read -r -s -p "VPS-specific fine-grained PAT: " github_token
  printf '\n' >&2
fi

[[ "${github_token}" == github_pat_* ]] || die "expected a fine-grained PAT beginning with github_pat_; refusing a broader or unknown credential type"

state_dir="$(dirname "${token_state_file}")"
temporary_token="$(mktemp "${state_dir}/github-token.tmp.XXXXXX")"
chmod 0600 "${temporary_token}"
printf '%s' "${github_token}" >"${temporary_token}"
mv "${temporary_token}" "${token_state_file}"

cleanup_failed_provisioning() {
  local status=$?
  unset github_token
  if (( status != 0 )); then
    log "workspace provisioning failed; ${token_state_file} was retained so 03-destroy.sh can revoke the credential"
  fi
  return "${status}"
}
trap cleanup_failed_provisioning EXIT

if ! repo_json="$(GH_TOKEN="${github_token}" gh api "repos/${repository}" 2>/dev/null)"; then
  die "the token cannot access ${repository}; verify its resource owner, repository selection, approval, and expiration"
fi
[[ "$(jq -r '.full_name' <<<"${repo_json}")" == "${repository}" ]] || die "GitHub returned an unexpected repository for the supplied token"
github_actor="$(GH_TOKEN="${github_token}" gh api user --jq .login 2>/dev/null)" || die "could not identify the GitHub account associated with the token"

log "installing the repository-scoped GitHub credential on the VPS"
printf '%s' "$(cat "${token_state_file}")" | remote_ssh 'umask 077; cat > /home/agent/.config/vps-codex/github-token'
unset github_token

log "cloning ${repository} and checking out ${WORK_BRANCH}"
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
checkout_dir="/workspace/projects/${repository##*/}"

[[ "$(id -un)" == agent ]] || { echo "workspace provisioning must run as agent" >&2; exit 1; }
[[ -s "${HOME}/.config/vps-codex/github-token" ]] || { echo "GitHub credential is missing" >&2; exit 1; }
[[ ! -e "${checkout_dir}" ]] || { echo "checkout already exists: ${checkout_dir}" >&2; exit 1; }

# Point Git at the root-owned wrapper rather than /usr/bin/gh. The wrapper
# loads this VPS's protected token file for every later credential request.
git config --global credential.helper '!/usr/local/bin/gh auth git-credential'
git clone --branch "${base_branch}" --single-branch "https://github.com/${repository}.git" "${checkout_dir}"
cd "${checkout_dir}"
git config --local user.name "${git_name}"
git config --local user.email "${git_email}"
git switch -c "${work_branch}"
git ls-remote --exit-code origin "refs/heads/${base_branch}" >/dev/null
/usr/local/bin/gh repo view "${repository}" --json nameWithOwner --jq .nameWithOwner >/dev/null
REMOTE

checkout_dir="/workspace/projects/${repository##*/}"
printf '\nGitHub workspace ready\n  GitHub actor: %s\n  repository: %s\n  checkout: %s\n  branch: %s\n  token retained for revocation: %s\n\nThe VPS can now run git and gh operations without the provisioning host.\nConnect with:\n  ssh -F %q %q\n' \
  "${github_actor}" "${repository}" "${checkout_dir}" "${WORK_BRANCH}" "${token_state_file}" \
  "${SSH_CONFIG_FILE}" "${SSH_ALIAS}"
