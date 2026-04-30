//! Universal pause / resume of "whatever is currently playing" on macOS.
//!
//! Uses [`nowplaying-cli`](https://github.com/kirtan-shah/nowplaying-cli)
//! which talks to Apple's private `MediaRemote.framework` to operate on
//! whichever app currently owns Now Playing. `AppleScript` synthetic key
//! codes do **not** work for this — they send F8 as a normal keystroke,
//! never reach `MRMediaRemoteSendCommand`, and macOS does nothing.
//!
//! Install once on the machine running the bot:
//!
//! ```sh
//! brew install nowplaying-cli
//! ```
//!
//! Without it, [`toggle_play_pause`] returns `false` and the bot logs a
//! warning. Music will keep playing under the viewer track.

use tokio::process::Command;

const BIN: &str = "nowplaying-cli";

/// Pause whatever is currently playing **only if** Now Playing reports a
/// `playbackRate` of `1` (= actively playing). Returns `true` if we sent a
/// toggle, `false` if nothing was playing or the call failed. The caller
/// uses the boolean to decide whether to re-toggle on queue drain.
pub async fn pause_if_playing() -> bool {
    if !is_playing().await {
        tracing::info!(
            "Now Playing is idle/paused, skipping pause toggle (avoids resuming user's audio)"
        );
        return false;
    }
    if !toggle_play_pause().await {
        tracing::warn!("nowplaying-cli togglePlayPause failed");
        return false;
    }
    true
}

/// Resume whatever was playing before. The caller is responsible for
/// having recorded that we paused something.
pub async fn resume() {
    if !toggle_play_pause().await {
        tracing::warn!("nowplaying-cli togglePlayPause failed (resume)");
    }
}

/// Send a single toggle. Most callers want [`pause_if_playing`] or
/// [`resume`] which keep the bot's state machine honest.
async fn toggle_play_pause() -> bool {
    Command::new(BIN)
        .arg("togglePlayPause")
        .output()
        .await
        .is_ok_and(|o| o.status.success())
}

/// Returns `true` iff `nowplaying-cli get playbackRate` returns exactly
/// "1". Anything else (paused, stopped, no Now Playing source, missing
/// binary, error) is treated as "not playing".
pub async fn is_playing() -> bool {
    let Ok(output) = Command::new(BIN)
        .arg("get")
        .arg("playbackRate")
        .output()
        .await
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    raw.trim() == "1"
}

/// Probe whether `nowplaying-cli` is callable. Logs a warning when it is
/// not so the operator knows the auto-pause feature is degraded.
pub async fn check_available() -> bool {
    let ok = Command::new(BIN)
        .arg("get")
        .arg("title")
        .output()
        .await
        .is_ok_and(|o| o.status.success());
    if !ok {
        tracing::warn!(
            "nowplaying-cli not found on PATH — auto pause/resume of your music \
             will not work. Install with `brew install nowplaying-cli`."
        );
    }
    ok
}

/// Brief wait we leave between pausing the user's music and starting the
/// viewer track, so the system audio buffer drains and the two streams
/// don't overlap on the listener's headphones.
pub const PAUSE_TO_PLAY_DELAY: std::time::Duration = std::time::Duration::from_millis(350);
