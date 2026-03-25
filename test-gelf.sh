#!/bin/bash
# Send test GELF messages to verify the logmon MCP server is receiving logs.
# Usage: ./test-gelf.sh [port]
#   port: GELF UDP port (default: 12201)

PORT=${1:-12201}
HOST="localhost"

send_gelf() {
    echo "$1" | nc -u -w0 "$HOST" "$PORT"
}

echo "Sending test GELF messages to $HOST:$PORT..."

# Normal info messages
send_gelf '{"version":"1.1","host":"test-app","short_message":"Application started","level":6,"facility":"test::main","_request_id":"req-001"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Connected to database","level":6,"facility":"test::db","_request_id":"req-002"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Loading configuration from /etc/app.toml","level":7,"facility":"test::config"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Listening on 0.0.0.0:8080","level":6,"facility":"test::server"}'
sleep 0.1

# Warning
send_gelf '{"version":"1.1","host":"test-app","short_message":"Connection pool nearly full (95%)","level":4,"facility":"test::db","_pool_size":"95"}'
sleep 0.1

# More info
send_gelf '{"version":"1.1","host":"test-app","short_message":"Processing request GET /api/status","level":6,"facility":"test::http","_method":"GET","_path":"/api/status"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Cache miss for key user:42","level":7,"facility":"test::cache","_key":"user:42"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Request completed in 23ms","level":6,"facility":"test::http","_duration_ms":"23"}'
sleep 0.1

# Error — should trigger notification
send_gelf '{"version":"1.1","host":"test-app","short_message":"Connection refused to redis://cache:6379","level":3,"facility":"test::cache","full_message":"Error: Connection refused (os error 111)\n  at src/cache.rs:42\n  at src/handler.rs:128","file":"cache.rs","line":42}'
sleep 0.1

# Post-error messages (should be captured by post-trigger window)
send_gelf '{"version":"1.1","host":"test-app","short_message":"Retrying cache connection (attempt 1/3)","level":4,"facility":"test::cache"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Retrying cache connection (attempt 2/3)","level":4,"facility":"test::cache"}'
sleep 0.1

send_gelf '{"version":"1.1","host":"test-app","short_message":"Cache connection restored","level":6,"facility":"test::cache"}'
sleep 0.1

echo "Done! Sent 12 messages (8 info, 1 warning, 1 error, 2 retries)."
echo ""
echo "In Claude Code, try:"
echo '  - "check the logs" or "get log status"'
echo '  - "show me recent errors"'
echo '  - "what happened around the cache error?"'
