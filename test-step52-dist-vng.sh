#!/bin/bash
# Host-side orchestration for two independent vng guests. Privileged filesystem
# operations live exclusively in test-step52-dist-node-vng.sh inside each guest.
set -euo pipefail

: "${REDIS_URL:?set REDIS_URL to a disposable Redis database}"
: "${S3_ENDPOINT:?set S3_ENDPOINT}"
: "${S3_BUCKET:?set S3_BUCKET to an existing bucket}"
: "${AWS_ACCESS_KEY_ID:?set AWS_ACCESS_KEY_ID}"
: "${AWS_SECRET_ACCESS_KEY:?set AWS_SECRET_ACCESS_KEY}"

run_id="$(date +%s)-$$-$RANDOM"
coord="$PWD/.step52-dist-$run_id"
mkdir -p "$coord"
pid_a=
pid_b=

cleanup() {
	set +e
	test -z "$pid_a" || kill "$pid_a" 2>/dev/null || true
	test -z "$pid_b" || kill "$pid_b" 2>/dev/null || true
	rm -rf "$coord"
}
trap cleanup EXIT

guest_command() {
	local role=$1
	local command=env
	local name value quoted

	for name in STEP52_RUN_ID STEP52_COORD REDIS_URL S3_ENDPOINT S3_BUCKET \
		AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION AWS_SESSION_TOKEN \
		STEP53_NOTIFY; do
		case "$name" in
		STEP52_RUN_ID) value=$run_id ;;
		STEP52_COORD) value=$coord ;;
		*) value=${!name-} ;;
		esac
		test -z "$value" && continue
		printf -v quoted '%q' "$value"
		command+=" $name=$quoted"
	done
	printf -v quoted '%q' "$role"
	command+=" STEP52_ROLE=$quoted"
	printf -v quoted '%q' "$PWD/test-step52-dist-node-vng.sh"
	command+=" $quoted"
	printf '%s' "$command"
}

command_a=$(guest_command A)
command_b=$(guest_command B)

vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec "$command_a" \
	>"$coord/node-a.log" 2>&1 &
pid_a=$!
vng --run --network user --rwdir "$PWD" --cwd "$PWD" --exec "$command_b" \
	>"$coord/node-b.log" 2>&1 &
pid_b=$!

set +e
wait "$pid_a"
status_a=$?
pid_a=
wait "$pid_b"
status_b=$?
pid_b=
set -e

cat "$coord/node-a.log"
cat "$coord/node-b.log"
if test "$status_a" -ne 0 || test "$status_b" -ne 0; then
	echo "STEP52_DIST_FAIL: node_a=$status_a node_b=$status_b"
	exit 1
fi
grep -Fx STEP52_DIST_A_PASS "$coord/node-a.log" >/dev/null
grep -Fx STEP52_DIST_B_PASS "$coord/node-b.log" >/dev/null
grep -Fx STEP52_DIST_REMOTE_VISIBLE_PASS "$coord/node-b.log" >/dev/null
grep -Fx STEP52_DIST_DAEMON_FREE_NEW_HIT_PASS "$coord/node-b.log" >/dev/null
echo STEP52_DIST_TWO_NODE_PASS
