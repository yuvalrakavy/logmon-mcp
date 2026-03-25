#!/bin/bash
# Send test GELF messages to verify the logmon MCP server is receiving logs.
# Usage: ./test-gelf.sh [port] [protocol]
#   port:     GELF port (default: 12201)
#   protocol: "tcp" (default) or "udp"

PORT=${1:-12201}
PROTO=${2:-tcp}
HOST="localhost"

send_gelf_tcp() {
    # Accumulate all messages and send in one TCP connection
    TCP_PAYLOAD+="$1\0"
}

flush_tcp() {
    printf "$TCP_PAYLOAD" | nc "$HOST" "$PORT"
}

send_gelf_udp() {
    # nc -u -w0 is unreliable on macOS (closes before sending), use python3 instead
    python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.sendto(b'''$1''', ('$HOST', $PORT))
s.close()
"
}

TCP_PAYLOAD=""

echo "Sending test GELF messages to $HOST:$PORT via $PROTO..."

MESSAGES=(
  '{"version":"1.1","host":"test-app","short_message":"Application started","level":6,"facility":"test::main","_request_id":"req-001"}'
  '{"version":"1.1","host":"test-app","short_message":"Connected to database","level":6,"facility":"test::db","_request_id":"req-002"}'
  '{"version":"1.1","host":"test-app","short_message":"Loading configuration from /etc/app.toml","level":7,"facility":"test::config"}'
  '{"version":"1.1","host":"test-app","short_message":"Listening on 0.0.0.0:8080","level":6,"facility":"test::server"}'
  '{"version":"1.1","host":"test-app","short_message":"Connection pool nearly full (95%)","level":4,"facility":"test::db","_pool_size":"95"}'
  '{"version":"1.1","host":"test-app","short_message":"Processing request GET /api/status","level":6,"facility":"test::http","_method":"GET","_path":"/api/status"}'
  '{"version":"1.1","host":"test-app","short_message":"Cache miss for key user:42","level":7,"facility":"test::cache","_key":"user:42"}'
  '{"version":"1.1","host":"test-app","short_message":"Request completed in 23ms","level":6,"facility":"test::http","_duration_ms":"23"}'
  '{"version":"1.1","host":"test-app","short_message":"Connection refused to redis://cache:6379","level":3,"facility":"test::cache","full_message":"Error: Connection refused (os error 111)\\n  at src/cache.rs:42\\n  at src/handler.rs:128","file":"cache.rs","line":42}'
  '{"version":"1.1","host":"test-app","short_message":"Retrying cache connection (attempt 1/3)","level":4,"facility":"test::cache"}'
  '{"version":"1.1","host":"test-app","short_message":"Retrying cache connection (attempt 2/3)","level":4,"facility":"test::cache"}'
  '{"version":"1.1","host":"test-app","short_message":"Cache connection restored","level":6,"facility":"test::cache"}'
)

for msg in "${MESSAGES[@]}"; do
    if [ "$PROTO" = "udp" ]; then
        send_gelf_udp "$msg"
    else
        send_gelf_tcp "$msg"
    fi
done

if [ "$PROTO" = "tcp" ]; then
    flush_tcp
fi

echo "Done! Sent ${#MESSAGES[@]} messages (8 info, 1 warning, 1 error, 2 retries)."
echo ""
echo "In Claude Code, try:"
echo '  - "check the logs" or "get log status"'
echo '  - "show me recent errors"'
echo '  - "what happened around the cache error?"'
echo ""
echo "NOTE: If UDP doesn't work, the MCP server may be sandboxed."
echo "      Use TCP (default) or add localhost to the sandbox network allowlist."
