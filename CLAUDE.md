# twitchy — project rules

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
- Logs: `tracing` (level via `RUST_LOG`, default `twitchy=info`).
- No `unwrap()` / `expect()` in the runtime path — only in `main` for fatal init errors and in tests.
- Configuration: env (`.env` via `dotenvy`) + `config/rewards.toml`. No secret is ever logged (passwords, tokens).
- Twitch token persisted to `<state_dir>/token.json` (default `./.twitchy/`), covered by `.gitignore`.
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

## Out of scope (explicit non-goals)

- No HTTP webhook EventSub transport — WebSocket only.
- No support for Hue Bluetooth — every lamp goes through `/api/zigbee`.
- No `MaisonApi` trait abstraction while there is a single implementation.
- The Maison password is read from `.env` once at startup; it is never stored elsewhere.
- No web UI — configuration is exclusively `config/rewards.toml`.
