# List available recipes
default:
    @just --list

# Start the TUI in a detached tmux session named `hifi` (no attach)
start:
    @echo "==> Building hifi (release)…"
    cargo build --release
    @if tmux has-session -t hifi 2>/dev/null; then echo "hifi: already running"; else tmux new-session -d -s hifi 'target/release/hifi' && echo "hifi: started (use 'just attach' to interact)"; fi

# Start (if needed) and attach to the TUI; detach with Ctrl-b d
run:
    @echo "==> Building hifi (release)… (first build can take ~30s; rebuilds are quick)"
    cargo build --release
    @echo "==> Launching hifi in tmux — detach with Ctrl-b d, stop with 'just stop'"
    @tmux new-session -A -s hifi 'target/release/hifi'

# Attach to a running hifi tmux session
attach:
    tmux attach -t hifi

# Kill the tmux session (stops the app)
stop:
    -tmux kill-session -t hifi

# Show whether the session is running and recent log entries
status:
    @if tmux has-session -t hifi 2>/dev/null; then echo "hifi: RUNNING"; else echo "hifi: stopped"; fi
    @echo ""
    @just logs 20

# Capture the TUI's current screen (useful for inspection without attaching)
peek:
    tmux capture-pane -p -t hifi

# Build & run inline (no tmux) — for when you don't want a session
run-fg:
    cargo run --release --bin hifi

# Debug build run inline
run-debug:
    cargo run --bin hifi

# Release build without running
build:
    cargo build --release

# Fast type-check
check:
    cargo check

# Format
fmt:
    cargo fmt

# Lint
clippy:
    cargo clippy --all-targets -- -D warnings

# Tests
test:
    cargo test

# Forget cached Spotify token — next run re-does PKCE in browser
reauth:
    rm -f hifi-auth.json

# Tail the most recent log events from the SQLite log
logs n="50":
    sqlite3 -header -column hifi.log.sqlite "SELECT ts, kind, request_id, method, status, latency_ms, substr(coalesce(detail,body),1,120) AS info FROM events ORDER BY id DESC LIMIT {{n}};"

# Open the log DB in sqlite3 shell
logs-shell:
    sqlite3 hifi.log.sqlite

# Delete the SQLite log database
logs-clear:
    rm -f hifi.log.sqlite hifi.log.sqlite-shm hifi.log.sqlite-wal

# Show Spotify's view of current playback + devices (run while TUI is up)
diag:
    cargo run --release --bin hifi-diag

# Send play to the "hifi" device and poll /me/player for 10s
diag-play uri:
    cargo run --release --bin hifi-diag -- play {{uri}}

# Wipe build artifacts
clean:
    cargo clean
