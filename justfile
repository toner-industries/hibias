# List available recipes
default:
    @just --list

# Start the TUI in a detached tmux session named `hibias` (no attach)
start:
    @echo "==> Building hibias (release)…"
    cargo build --release
    @if tmux has-session -t hibias 2>/dev/null; then echo "hibias: already running"; else tmux new-session -d -x 96 -y 41 -s hibias 'target/release/hibias' && echo "hibias: started (use 'just attach' to interact)"; fi

# Start (if needed) and attach to the TUI; detach with Ctrl-b d
run:
    @echo "==> Building hibias (release)… (first build can take ~30s; rebuilds are quick)"
    cargo build --release
    @echo "==> Launching hibias in tmux — detach with Ctrl-b d, stop with 'just stop'"
    @tmux new-session -A -s hibias 'target/release/hibias'

# Attach to a running hibias tmux session
attach:
    tmux attach -t hibias

# Kill the tmux session (stops the app)
stop:
    -tmux kill-session -t hibias

# Show whether the session is running and recent log entries
status:
    @if tmux has-session -t hibias 2>/dev/null; then echo "hibias: RUNNING"; else echo "hibias: stopped"; fi
    @echo ""
    @just logs 20

# Capture the TUI's current screen (useful for inspection without attaching)
peek:
    tmux capture-pane -p -t hibias

# Build & run inline (no tmux) — for when you don't want a session
run-fg:
    cargo run --release --bin hibias

# Debug build run inline
run-debug:
    cargo run --bin hibias

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
    rm -f hibias-auth.json

# Tail the most recent log events from the SQLite log
logs n="50":
    sqlite3 -header -column hibias.log.sqlite "SELECT ts, kind, request_id, method, status, latency_ms, substr(coalesce(detail,body),1,120) AS info FROM events ORDER BY id DESC LIMIT {{n}};"

# Open the log DB in sqlite3 shell
logs-shell:
    sqlite3 hibias.log.sqlite

# Delete the SQLite log database
logs-clear:
    rm -f hibias.log.sqlite hibias.log.sqlite-shm hibias.log.sqlite-wal

# Show Spotify's view of current playback + devices (run while TUI is up)
diag:
    cargo run --release --bin hibias-diag

# Send play to the "hibias" device and poll /me/player for 10s
diag-play uri:
    cargo run --release --bin hibias-diag -- play {{uri}}

# Wipe build artifacts
clean:
    cargo clean
