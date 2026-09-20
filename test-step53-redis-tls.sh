#!/bin/bash
# Step 53 REDIS-HARDEN: self-signed-CA rediss:// integration gate.
set -euo pipefail

: "${REDIS_URL:?set REDIS_URL to a disposable redis:// database}"

tmpdir=$(mktemp -d /tmp/kestrel-step53-tls-XXXXXX)
port=$((20000 + ($$ % 20000)))
tls_pid=
upstream=$(printf '%s' "$REDIS_URL" | sed -E 's#^redis://([^@/]+@)?([^/]+)/?.*$#\2#')
credentials=$(printf '%s' "$REDIS_URL" | sed -n -E 's#^redis://([^@/]+@).*$#\1#p')
database=$(printf '%s' "$REDIS_URL" | sed -n -E 's#^.*/([0-9]+)$#\1#p')
test -n "$database" || database=0
tls_url="rediss://${credentials}127.0.0.1:$port/$database"

cleanup() {
	set +e
	if test -n "$tls_pid"; then
		kill "$tls_pid" 2>/dev/null || true
		wait "$tls_pid" 2>/dev/null || true
	fi
	rm -rf "$tmpdir"
}
trap cleanup EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
	-keyout "$tmpdir/ca.key" -out "$tmpdir/ca.crt" \
	-subj '/CN=KestrelFS Step53 Test CA' >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
	-keyout "$tmpdir/server.key" -out "$tmpdir/server.csr" \
	-subj '/CN=127.0.0.1' -addext 'subjectAltName=IP:127.0.0.1' \
	>/dev/null 2>&1
openssl x509 -req -days 1 -in "$tmpdir/server.csr" \
	-CA "$tmpdir/ca.crt" -CAkey "$tmpdir/ca.key" -CAcreateserial \
	-copy_extensions copy -out "$tmpdir/server.crt" >/dev/null 2>&1

socat \
	"OPENSSL-LISTEN:$port,bind=127.0.0.1,reuseaddr,fork,cert=$tmpdir/server.crt,key=$tmpdir/server.key,cafile=$tmpdir/ca.crt,verify=0" \
	"TCP:$upstream" >"$tmpdir/tls-proxy.log" 2>&1 &
tls_pid=$!

for _ in $(seq 1 100); do
	if redis-cli -u "$tls_url" --cacert "$tmpdir/ca.crt" PING \
		2>/dev/null | grep -Fx PONG >/dev/null; then
		break
	fi
	sleep 0.05
done
if ! kill -0 "$tls_pid" 2>/dev/null; then
	cat "$tmpdir/tls-proxy.log"
	exit 1
fi
if ! redis-cli -u "$tls_url" --cacert "$tmpdir/ca.crt" PING | \
	grep -Fx PONG >/dev/null; then
	cat "$tmpdir/tls-proxy.log"
	exit 1
fi

REDIS_TLS_URL="$tls_url" \
	REDIS_TLS_CA_CERT="$tmpdir/ca.crt" \
	cargo test --manifest-path daemon/Cargo.toml \
		redis_tls_url_gated_self_signed_ca -- --nocapture
echo STEP53_REDIS_TLS_PASS
