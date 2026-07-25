#!/usr/bin/env bash

set -euo pipefail

if (( $# != 8 )); then
  echo "usage: remote-worker.sh PHASE REPOSITORY BASE_BRANCH WORK_BRANCH TASK_FILE PR_TITLE GIT_NAME GIT_EMAIL" >&2
  exit 2
fi

phase="$1"
repository="$2"
base_branch="$3"
work_branch="$4"
task_file="$5"
pr_title="$6"
git_name="$7"
git_email="$8"

[[ "${phase}" =~ ^(prepare|run|publish)$ ]] || { echo "invalid phase" >&2; exit 2; }
[[ "${work_branch}" =~ ^[A-Za-z0-9._/-]+$ ]] || { echo "invalid work branch" >&2; exit 2; }
[[ -s "${task_file}" ]] || { echo "task file is empty or missing" >&2; exit 2; }

job_id="$(basename "${task_file}" .md)"
job_dir="/workspace/jobs/${job_id}"
checkout_dir="${job_dir}/repository"

[[ "$(id -un)" == "agent" ]] || { echo "remote worker must run as agent" >&2; exit 1; }
install -d -m 0755 "${job_dir}"

exec env -i \
    HOME=/home/agent \
    USER=agent \
    LOGNAME=agent \
    PATH=/home/agent/.local/bin:/usr/local/bin:/usr/bin:/bin \
    PHASE="${phase}" \
    REPOSITORY="${repository}" \
    BASE_BRANCH="${base_branch}" \
    WORK_BRANCH="${work_branch}" \
    TASK_FILE="${task_file}" \
    PR_TITLE="${pr_title}" \
    GIT_AUTHOR_NAME="${git_name}" \
    GIT_AUTHOR_EMAIL="${git_email}" \
    JOB_DIR="${job_dir}" \
    CHECKOUT_DIR="${checkout_dir}" \
  bash <<'WORKER'
set -euo pipefail

token_file="${HOME}/.config/vps-codex/github-token"
base_sha_file="${JOB_DIR}/base-sha"
result_file="${JOB_DIR}/result.json"
events_file="${JOB_DIR}/codex-events.jsonl"
final_message_file="${JOB_DIR}/codex-final.md"

use_github_token() {
  [[ -s "${token_file}" ]] || { echo "GitHub token is missing for ${PHASE} phase" >&2; exit 1; }
  export GH_TOKEN="$(cat "${token_file}")"
  gh auth setup-git >/dev/null
}

remove_github_token() {
  unset GH_TOKEN || true
  rm -f "${token_file}"
}

case "${PHASE}" in
  prepare)
    [[ ! -e "${CHECKOUT_DIR}" ]] || { echo "checkout already exists" >&2; exit 1; }
    trap remove_github_token EXIT
    use_github_token
    git config --global user.name "${GIT_AUTHOR_NAME}"
    git config --global user.email "${GIT_AUTHOR_EMAIL}"
    echo "Cloning ${REPOSITORY} at ${BASE_BRANCH}" >&2
    gh repo clone "${REPOSITORY}" "${CHECKOUT_DIR}" -- --branch "${BASE_BRANCH}" --single-branch
    cd "${CHECKOUT_DIR}"
    git switch -c "${WORK_BRANCH}"
    git rev-parse HEAD >"${base_sha_file}"
    ;;

  run)
    [[ -d "${CHECKOUT_DIR}/.git" && -s "${base_sha_file}" ]] || { echo "prepare phase has not completed" >&2; exit 1; }
    [[ ! -e "${token_file}" ]] || { echo "GitHub token must not be present during Codex execution" >&2; exit 1; }
    cd "${CHECKOUT_DIR}"

    prompt_file="${JOB_DIR}/codex-prompt.md"
    cat >"${prompt_file}" <<'PROMPT'
You are implementing a development task in a disposable remote VPS checkout.

Work autonomously through the complete engineering loop:

1. Read the repository guidance and inspect the existing architecture before editing.
2. Implement the requested feature using existing project conventions and the smallest correct change.
3. Install dependencies if needed.
4. Run the narrowest meaningful tests, type checks, lint checks, and/or build for the changed behavior.
5. Fix failures caused by your changes. Do not hide failures or weaken tests.
6. Leave all intended source and test changes in the working tree.
7. Do not push, create a pull request, rewrite history, or inspect/expose credentials; the outer worker handles Git publication.
8. In your final response, summarize the implementation, list verification commands and outcomes, and identify any unresolved issue.

The requested task follows.

---
PROMPT
    cat "${TASK_FILE}" >>"${prompt_file}"

    echo "Running Codex without GitHub credentials in workspace-write mode" >&2
    codex exec \
      --ephemeral \
      --ignore-user-config \
      --sandbox workspace-write \
      --config sandbox_workspace_write.network_access=true \
      --json \
      --output-last-message "${final_message_file}" \
      - <"${prompt_file}" | tee "${events_file}"

    if [[ -n "$(git status --porcelain)" ]]; then
      git add -A
      git commit -m "${PR_TITLE}"
    fi

    base_sha="$(cat "${base_sha_file}")"
    [[ "$(git rev-list --count "${base_sha}..HEAD")" != "0" ]] || {
      echo "Codex completed without producing a commit" >&2
      exit 1
    }
    ;;

  publish)
    [[ -d "${CHECKOUT_DIR}/.git" && -s "${base_sha_file}" ]] || { echo "run phase has not completed" >&2; exit 1; }
    trap remove_github_token EXIT
    use_github_token
    cd "${CHECKOUT_DIR}"
    base_sha="$(cat "${base_sha_file}")"
    [[ "$(git rev-list --count "${base_sha}..HEAD")" != "0" ]] || { echo "no commit to publish" >&2; exit 1; }

    git push --set-upstream origin "${WORK_BRANCH}"

    pr_body="${JOB_DIR}/pr-body.md"
    {
      echo "This draft PR was implemented by Codex on an ephemeral VPS."
      echo
      echo "## Agent report"
      echo
      if [[ -s "${final_message_file}" ]]; then
        cat "${final_message_file}"
      else
        echo "Codex did not return a final textual report."
      fi
      echo
      echo "## Change summary"
      echo
      echo '```text'
      git diff --stat "${base_sha}..HEAD"
      echo '```'
    } >"${pr_body}"

    if ! pr_url="$(gh pr create \
      --draft \
      --base "${BASE_BRANCH}" \
      --head "${WORK_BRANCH}" \
      --title "${PR_TITLE}" \
      --body-file "${pr_body}")"; then
      pr_url="$(gh pr view "${WORK_BRANCH}" --json url --jq .url)"
    fi

    jq -n \
      --arg status succeeded \
      --arg repository "${REPOSITORY}" \
      --arg baseBranch "${BASE_BRANCH}" \
      --arg workBranch "${WORK_BRANCH}" \
      --arg commit "$(git rev-parse HEAD)" \
      --arg pullRequest "${pr_url}" \
      '{
        status: $status,
        repository: $repository,
        baseBranch: $baseBranch,
        workBranch: $workBranch,
        commit: $commit,
        pullRequest: $pullRequest
      }' | tee "${result_file}"
    ;;
esac
WORKER
