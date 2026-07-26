#!/usr/bin/env bash

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/snapshot-local-state.XXXXXX")"
STATE_DIR="${TEST_DIR}/state"
FAKE_BIN="${TEST_DIR}/bin"
SSH_CALLED="${TEST_DIR}/ssh-called"
CONFIG_FILE="${TEST_DIR}/config.env"

cleanup() {
  rm -rf "${TEST_DIR}"
}
trap cleanup EXIT

fail() {
  printf 'not ok - %s\n' "$*" >&2
  exit 1
}

mkdir -p "${STATE_DIR}" "${FAKE_BIN}"
printf 'VPS_INSTANCE_STATE_DIR=%q\n' "${STATE_DIR}" >"${CONFIG_FILE}"
cat >"${FAKE_BIN}/ssh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${SSH_CALLED}"
EOF
chmod +x "${FAKE_BIN}/ssh"

write_setup() {
  local instance_id="$1"
  mkdir -p "${STATE_DIR}/${instance_id}"
  cat >"${STATE_DIR}/${instance_id}/current.env.setup" <<'EOF'
SETUP_REPOSITORY=owner/repository
SETUP_BASE_BRANCH=main
SETUP_WORK_BRANCH=work
EOF
}

write_active() {
  local instance_id="$1"
  write_setup "${instance_id}"
  cat >"${STATE_DIR}/${instance_id}/current.env" <<EOF
DROPLET_ID=101
DROPLET_IP=192.0.2.10
SSH_CONFIG_FILE=${TEST_DIR}/ssh_config
SSH_ALIAS=test-worker
EOF
}

write_transition() {
  local instance_id="$1"
  local kind="$2"
  local phase="$3"
  local snapshot_id="$4"
  write_setup "${instance_id}"
  cat >"${STATE_DIR}/${instance_id}/current.env.transition" <<EOF
TRANSITION_VERSION=1
TRANSITION_KIND=${kind}
TRANSITION_PHASE=${phase}
OPERATION_ID=test-operation
PAUSE_OPERATION_ID=pause-operation
TRANSITION_STARTED_AT=2026-01-01T00:00:00Z
SNAPSHOT_ID=${snapshot_id}
SNAPSHOT_NAME=snapshot-test
SOURCE_DROPLET_ID=101
SOURCE_DROPLET_NAME=worker-test
SOURCE_REGION=nyc3
SOURCE_SIZE=s-4vcpu-8gb
SSH_HOST_ED25519_PUBLIC_KEY=ssh-ed25519\ test-key
TARGET_DROPLET_NAME=worker-test
TARGET_REGION=nyc3
TARGET_SIZE=s-4vcpu-8gb
TARGET_LIFECYCLE_TAG=resume-test
TARGET_DROPLET_ID=202
TARGET_DROPLET_IP=192.0.2.20
EOF
}

assert_list_row() {
  local expected="$1"
  grep -Fqx "${expected}" "${TEST_DIR}/list.out" || fail "missing list row: ${expected}"
}

write_active active
write_active contradictory
cat >"${STATE_DIR}/contradictory/current.env.paused" <<'EOF'
PAUSED_STATE_VERSION=1
SNAPSHOT_ID=701
EOF
write_setup paused
cat >"${STATE_DIR}/paused/current.env.paused" <<'EOF'
PAUSED_STATE_VERSION=1
SNAPSHOT_ID=700
EOF

phases='pause pausing-quiescing 801
pause pausing-shutdown 802
pause pausing-snapshot 803
pause pausing-delete-pending 804
resume resuming-allocation 805
resume resuming-recovery 806
resume active-snapshot-cleanup-pending 807'
while read -r kind phase snapshot_id; do
  write_transition "${phase}" "${kind}" "${phase}" "${snapshot_id}"
done <<EOF
${phases}
EOF
write_transition invalid pause resuming-recovery 999

VPS_CODEX_CONFIG="${CONFIG_FILE}" "${REPO_DIR}/vps_list.sh" >"${TEST_DIR}/list.out"
header="$(printf 'INSTANCE\tSTATUS\tDROPLET_ID\tIP\tSNAPSHOT_ID\tSOURCE\tBRANCH')"
[[ "$(head -n 1 "${TEST_DIR}/list.out")" == "${header}" ]] || fail "incorrect list header"
assert_list_row "$(printf 'active\tactive\t101\t192.0.2.10\t-\towner/repository@main\twork')"
assert_list_row "$(printf 'contradictory\tinvalid-terminal-state\t-\t-\t-\towner/repository@main\twork')"
assert_list_row "$(printf 'paused\tpaused\t-\t-\t700\towner/repository@main\twork')"
while read -r kind phase snapshot_id; do
  assert_list_row "$(printf '%s\t%s\t202\t192.0.2.20\t%s\towner/repository@main\twork' "${phase}" "${phase}" "${snapshot_id}")"
done <<EOF
${phases}
EOF
assert_list_row "$(printf 'invalid\tinvalid-transition-state\t-\t-\t-\towner/repository@main\twork')"

assert_shell_refused() {
  local instance_id="$1"
  local output
  local status
  rm -f "${SSH_CALLED}"
  set +e
  output="$(PATH="${FAKE_BIN}:${PATH}" SSH_CALLED="${SSH_CALLED}" VPS_CODEX_CONFIG="${CONFIG_FILE}" \
    "${REPO_DIR}/vps_shell.sh" "${instance_id}" 2>&1)"
  status=$?
  set -e
  (( status != 0 )) || fail "shell unexpectedly accepted ${instance_id}"
  [[ ! -e "${SSH_CALLED}" ]] || fail "shell invoked ssh for ${instance_id}"
  [[ -n "${output}" ]] || fail "shell refusal for ${instance_id} had no explanation"
}

assert_shell_refused paused
while read -r kind phase snapshot_id; do
  [[ "${phase}" == active-snapshot-cleanup-pending ]] && continue
  assert_shell_refused "${phase}"
done <<EOF
${phases}
EOF

write_active active-snapshot-cleanup-pending
touch "${TEST_DIR}/ssh_config"
rm -f "${SSH_CALLED}"
PATH="${FAKE_BIN}:${PATH}" SSH_CALLED="${SSH_CALLED}" VPS_CODEX_CONFIG="${CONFIG_FILE}" \
  "${REPO_DIR}/vps_shell.sh" active-snapshot-cleanup-pending
[[ -f "${SSH_CALLED}" ]] || fail "cleanup-pending shell did not invoke fake ssh"
[[ "$(cat "${SSH_CALLED}")" == "-F ${TEST_DIR}/ssh_config test-worker" ]] || fail "fake ssh received unexpected arguments"

# DigitalOcean snapshot IDs and source resource IDs are JSON strings.
# shellcheck source=../lib.sh
# shellcheck disable=SC1091
source "${REPO_DIR}/lib.sh"
snapshot_fixture='[{"id":"700","name":"snapshot-test","created_at":"2026-01-01T00:00:00Z","regions":["nyc3"],"resource_id":"101","resource_type":"droplet","min_disk_size":25}]'
validate_snapshot_json "${snapshot_fixture}" 700 snapshot-test 101 nyc3 50 || fail "quoted provider snapshot IDs were rejected"
if validate_snapshot_json "${snapshot_fixture}" 701 snapshot-test 101 nyc3 50; then
  fail "wrong snapshot ID was accepted"
fi

printf 'ok - snapshot local-state list and shell behavior\n'
