# homie — project rules

## Goal

Rust Twitch bot that listens via EventSub WebSocket to channel-point redemptions and chat messages, and triggers actions on the Maison home API (https://github.com/uplg/maison) to drive Zigbee lamps, the Mitsubishi AC (Broadlink IR) and the Tuya pet feeder.

## Stack

- Rust **edition 2024** (`rust-version = "1.85"`).
- Async runtime: **tokio** multi-thread.
- HTTP: **reqwest** with rustls.
- Twitch: **`twitch_api 0.7.x`** + **`twitch_oauth2 0.15.x`** (re-exported via `twitch_api::twitch_oauth2`).
- EventSub transport: **WebSocket only** (`tokio-tungstenite` + helpers from `twitch_api::eventsub`). No public webhook.

## Dependencies

- Always pick the **latest stable version** on crates.io that is compatible with the rest of the dependency graph. `reqwest` is currently pinned to `0.12.x` because that is the version `twitch_api`/`twitch_oauth2` build their `Client` trait against; revisit when those crates move to `reqwest` `0.13`.
- No git/path dependencies unless explicitly justified in the commit that introduces them.
- After adding or upgrading a dependency: `cargo update -p <crate>` then re-run the quality gate below.

## Quality gate — run at the end of EVERY phase

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

A phase is only complete when all three commands pass clean. Fix clippy warnings rather than silencing them with `#[allow(...)]`, unless there is a documented reason that explains why.

## Tests

- Unit tests live in the module (`#[cfg(test)] mod tests`).
- Integration tests live in `tests/`:
  - `tests/maison_client.rs` mocks the Maison API with **wiremock**.
  - `tests/config.rs` checks TOML deserialization.
  - `tests/rewards_dispatch.rs` checks the reward → action mapping against fixture EventSub frames.
- **No** test hits Twitch or Maison over the network — everything goes through mocks or static JSON fixtures.

## Conventions

- Errors: a single `crate::Error` (thiserror), `pub type Result<T> = std::result::Result<T, Error>`.
- Logs: `tracing` (level via `RUST_LOG`, default `homie=info`).
- No `unwrap()` / `expect()` in the runtime path — only in `main` for fatal init errors and in tests.
- Configuration: env (`.env` via `dotenvy`) + `config/rewards.toml`. No secret is ever logged (passwords, tokens).
- Twitch token persisted to `<state_dir>/token.json` (default `./.homie/`), covered by `.gitignore`.
- The whole codebase, including comments and string literals, is in **English**. Reward titles in `config/rewards.toml` are the only exception: they must match exactly the strings the streamer configured on Twitch, in whatever language.

## Maison API reference

- Base URL: `${MAISON_BASE_URL}/api` (e.g. `http://192.168.1.10:3033/api`).
- Login: `POST /auth/login` with body `{"username","password"}` -> JWT in the `maison_session` cookie.
- All subsequent calls use header `Authorization: Bearer <jwt>` (the Maison backend accepts the bearer in addition to the cookie).
- On a 401 response, perform `login()` once and retry the request.
- Endpoints used:
  - `/api/zigbee/lamps/...` (power, brightness, color, temperature, effect)
  - `/api/broadlink/mitsubishi/{codes,send}` — body in **camelCase** (`localIp`, etc.)
  - `/api/devices/{id}/feeder/feed` — body `{"portion": u64}`
- **No** call to `/api/hue-lamps` is made: the user's hardware is exclusively Zigbee.

## Twitch API reference

- Required scopes: `channel:read:redemptions`, `user:read:chat`, `user:write:chat`, `user:bot`, `channel:bot`.
- OAuth: **Device Code Flow only** (the bot runs with no HTTP server).
- EventSub subscriptions: `channel.channel_points_custom_reward_redemption.add`, `channel.chat.message`.

## Music queue (`yt_queue` module)

- Audio playback uses `rodio 0.22` (which itself wraps cpal + symphonia). One worker tokio task owns the rodio `Player`; chat handlers send commands via mpsc.
- Track downloads use the `yt-dlp` crate from the `Guilherme-j10/yt-dlp` fork (`develop` branch). Upstream `yt-dlp 2.7.x` on crates.io is broken: it transitively pins `lofty 0.23.x` which is fully yanked. Drop the git override the day a fixed crate version lands.
- The system-installed `yt-dlp` and `ffmpeg` binaries (e.g. via `brew install`) are required at runtime; the crate calls them as subprocesses.
- macOS coordination: pause/resume goes through `nowplaying-cli togglePlayPause` (`brew install nowplaying-cli`). That CLI calls Apple's private `MediaRemote.framework`, which is the only reliable way to operate on Now Playing from a shell. AppleScript synthetic key codes (`tell application "System Events" to key code 100`) look right but only send F8 as a regular keystroke — they never trigger media-routing. Hence the soft dep.
- Audio output: `cpal::Device` selection by substring match against `HOMIE_AUDIO_DEVICE` (case-insensitive). Without the env var we open the system default. Useful to route the bot through `BlackHole` so OBS can capture viewer-queue audio independently of the system mix. The bot logs available output devices at startup so the operator knows what to set.

## Out of scope (explicit non-goals)

- No HTTP webhook EventSub transport — WebSocket only.
- No support for Hue Bluetooth — every lamp goes through `/api/zigbee`.
- No `MaisonApi` trait abstraction while there is a single implementation.
- The Maison password is read from `.env` once at startup; it is never stored elsewhere.
- No web UI — configuration is exclusively `config/rewards.toml`.
- The music queue is macOS-only (the pause/resume logic uses `osascript`). Linux/Windows would need a separate backend.
