#!/bin/bash
# Step 54 A: retain the Step 53 two-node notify path, then fence one live
# daemon by removing its exact TTL session key and require its next mutation
# to fail closed.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

output=$(mktemp /tmp/kestrel-step54-lease-XXXXXX)
cleanup() {
	rm -f "$output"
}
trap cleanup EXIT

STEP53_NOTIFY=1 STEP54_LEASE=1 ./tests/test-step52-dist-vng.sh | tee "$output"
grep -Fx STEP52_DIST_REMOTE_VISIBLE_PASS "$output" >/dev/null
grep -Fx STEP53_DIST_NOTIFY_WAKE_PASS "$output" >/dev/null
grep -Fx STEP54_LEASE_FENCED_WRITER_PASS "$output" >/dev/null
echo STEP54_LEASE_HEARTBEAT_NOTIFY_PASS
echo STEP54_LEASE_EXPIRED_FAIL_CLOSED_PASS
echo STEP54_LEASE_PASS
