#!/bin/bash
# Step 53 DIST-NOTIFY: run Step 52's two independent vng guests and require
# Pub/Sub wakeup plus visibility below half of the retained 100 ms poll period.
set -euo pipefail

output=$(mktemp /tmp/kestrel-step53-notify-XXXXXX)
cleanup() {
	rm -f "$output"
}
trap cleanup EXIT

STEP53_NOTIFY=1 ./test-step52-dist-vng.sh | tee "$output"
grep -Fx STEP53_DIST_NOTIFY_WAKE_PASS "$output" >/dev/null
grep -Fx STEP53_DIST_NOTIFY_DAEMON_FREE_HIT_PASS "$output" >/dev/null
grep -E '^STEP53_DIST_NOTIFY_LATENCY_MS=([0-9]|[1-4][0-9])$' \
	"$output" >/dev/null
echo STEP53_DIST_NOTIFY_TWO_NODE_PASS
