#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"
load_config

state_existed_before=false
[[ -e "${VPS_STATE_FILE}" ]] && state_existed_before=true

cleanup() {
  local status=$?
  if [[ "${state_existed_before}" == false && -e "${VPS_STATE_FILE}" && "${KEEP_VPS:-0}" != 1 ]]; then
    if [[ -e "${VPS_STATE_FILE}.preserve" ]]; then
      printf 'WARNING: preserving the billable Droplet because refreshed Codex credentials may require recovery. See %s.\n' "${VPS_STATE_FILE}.preserve" >&2
    else
      "${SCRIPT_DIR}/03-destroy.sh" || printf 'WARNING: automatic Droplet cleanup failed; run 03-destroy.sh manually.\n' >&2
    fi
  fi
  return "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"${SCRIPT_DIR}/01-allocate.sh"
"${SCRIPT_DIR}/02-run-codex-job.sh"

if [[ "${KEEP_VPS:-0}" == 1 ]]; then
  printf 'KEEP_VPS=1; the Droplet remains allocated and billable. Run 03-destroy.sh when finished.\n' >&2
fi
