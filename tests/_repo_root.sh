# Shared bootstrap for scripts under tests/.
# Source from any tests/*.sh so relative paths to daemon/ and kestrelfs/ work.
# shellcheck shell=bash
if test -z "${KESTRELFS_REPO_ROOT:-}"; then
	_tests_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
	KESTRELFS_REPO_ROOT="$(CDPATH= cd -- "$_tests_dir/.." && pwd)"
	export KESTRELFS_REPO_ROOT
fi
CDPATH= cd -- "$KESTRELFS_REPO_ROOT"
