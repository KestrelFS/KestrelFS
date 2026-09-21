#!/bin/bash
# Step 53 DIST-NOTIFY: run Step 52's two independent vng guests and require
# Pub/Sub wakeup plus visibility below half of the retained 100 ms poll period.
set -euo pipefail
# shellcheck source=_repo_root.sh
# Keep cwd at repo root so daemon/ and kestrelfs/ relative paths stay stable.
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

output=$(mktemp /tmp/kestrel-step53-notify-XXXXXX)
cleanup() {
	rm -f "$output"
}
trap cleanup EXIT

STEP53_NOTIFY=1 ./tests/test-step52-dist-vng.sh | tee "$output"
grep -Fx STEP53_DIST_NOTIFY_WAKE_PASS "$output" >/dev/null
grep -Fx STEP53_DIST_NOTIFY_DAEMON_FREE_HIT_PASS "$output" >/dev/null
grep -E '^STEP53_DIST_NOTIFY_LATENCY_MS=([0-9]|[1-4][0-9])$' \
	"$output" >/dev/null
echo STEP53_DIST_NOTIFY_TWO_NODE_PASS
