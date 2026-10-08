#!/bin/bash
# Step 58 daemon-only fail-closed and metadata concurrency regression.
set -euo pipefail
# shellcheck source=_repo_root.sh
# shellcheck disable=SC1091
. "$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/_repo_root.sh"

output=$(mktemp /tmp/kestrel-step58-failclosed-XXXXXX)
cleanup() {
	rm -f "$output"
}
trap cleanup EXIT

cargo test --manifest-path daemon/Cargo.toml step58_ -- --nocapture 2>&1 | tee "$output"
grep -q STEP58_META_MEM_CONCURRENT_PASS "$output"
grep -q STEP58_META_PARALLEL_UNIT_PASS "$output"
grep -q STEP58_META_SAME_INODE_ORDER_PASS "$output"
grep -q STEP58_FAILCLOSED_CORRUPT_PASS "$output"
grep -q STEP58_FAILCLOSED_HALF_COMMIT_PASS "$output"
grep -q STEP58_FAILCLOSED_SEMANTIC_PASS "$output"
grep -q STEP58_FAILCLOSED_LANE_PASS "$output"
echo STEP58_FAILCLOSED_PASS
