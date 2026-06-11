//! Time and freshness helpers: the boot/staleness `should_accept` gate, the
//! pause/resume progress anchor, and device-error detection.

use super::*;
use crate::api::Playback;
use std::time::SystemTime;

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Decide whether to accept incoming playback data over our current state.
/// Rejects polled data older than our last local action — librespot frequently
/// fails to push state updates to Spotify Connect, so `/me/player` keeps
/// returning whichever device reported last (often something from a prior
/// session, with a stale timestamp).
pub fn should_accept(s: &AppState, incoming: Option<&Playback>) -> bool {
    let local = s.last_local_action_ms;
    // Boot mode: accept any non-empty payload so we display *something*
    // (a "Nothing playing" screen is the worst possible outcome). Reject
    // empty 204s — those are the well-known race where /me/player hasn't
    // caught up to our transfer yet. The recently-played seed (applied
    // via force=true) still wins because it lands before the first poll.
    if s.boot {
        return incoming.is_some();
    }
    match incoming {
        Some(pb) => {
            // Accept if Spotify's state is at least as new as our last action.
            // Missing timestamps are treated as 0 (never trust them over a
            // recent local action).
            pb.timestamp.unwrap_or(0) >= local
        }
        None => {
            // Spotify reports no active session. Only believe it if our last
            // local action wasn't recent — otherwise it's the well-known 204
            // we get right after a play because librespot hasn't reported.
            now_unix_ms().saturating_sub(local) > 60_000
        }
    }
}

/// Effective progress in ms given the current state — same calculation as the
/// UI's `displayed_progress`, but lives here so `toggle_playback` can freeze
/// the value before mutating `is_playing`.
pub fn displayed_progress_for_toggle(s: &AppState) -> u64 {
    let Some(pb) = &s.playback else {
        return 0;
    };
    let base = pb.progress_ms.unwrap_or(0);
    if !pb.is_playing {
        return base;
    }
    match s.last_poll {
        Some(poll) => base + poll.elapsed().as_millis() as u64,
        None => base,
    }
}

pub const DEVICE_OFFLINE_MSG: &str =
    "Connect device 'hifi' is offline — auto-reconnecting (or press ':' → reconnect)";

pub fn is_device_not_found(msg: &str) -> bool {
    msg.contains("Device not found") || msg.contains("\"status\" : 404")
}

/// Matches the `"<METHOD> <url>: <status> <reason>"` shape `send_logged`
/// errors carry for the transient 5xx family Spotify's Connect endpoints
/// are known to throw under no particular provocation.
pub fn is_transient_server_error(msg: &str) -> bool {
    [500, 502, 503, 504]
        .iter()
        .any(|c| msg.contains(&format!(": {c} ")))
}
