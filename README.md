# homie

Twitch bot that bridges **channel-point redemptions** and **chat commands** to a [Maison](https://github.com/uplg/maison) home API. Listen for redemptions over EventSub WebSocket, dispatch them to Zigbee lamps / Mitsubishi AC / Tuya pet feeder, and confirm in chat.

- **Stack**: Rust 2024, tokio, `twitch_api 0.7`, `twitch_oauth2 0.15` (Device Code Flow), `reqwest`, `tokio-tungstenite`.
- **No webhook**: pure WebSocket EventSub, runs anywhere with outbound HTTPS — no public IP, no port forwarding.
- **No DB**: configuration is one TOML file, the Twitch token is cached on disk.

## Prerequisites

- Rust toolchain (1.95+ for edition 2024). `rustup default stable` is enough on a recent install.
- A Maison instance reachable from where the bot runs (default port `:3033`), with valid `users.json` credentials.
- A Twitch account that owns the channel you want to drive (the broadcaster account).
- For the music queue (`!yt`): `yt-dlp` and `ffmpeg` on the PATH. On macOS: `brew install yt-dlp ffmpeg`. The bot runs without them but `!yt` will reject submissions with a friendly error.
- For the auto pause/resume of your music when a viewer track starts: `nowplaying-cli` on the PATH (`brew install nowplaying-cli`). Without it the bot still plays viewer tracks but won't pause anything. The bot logs a warning at startup if missing.
- For OBS to capture the bot's audio: a virtual audio device (e.g. [BlackHole](https://github.com/ExistentialAudio/BlackHole) — `brew install blackhole-2ch`). Combine it with your usual output via *Audio MIDI Setup → New Multi-Output Device* if you also want to hear the music. Then set `HOMIE_AUDIO_DEVICE=BlackHole` in `.env` and add a *macOS Audio Capture* / *Audio Output Capture* source in OBS pointing at BlackHole.

## Configuration walkthrough

The bot needs three pieces of configuration:

1. A registered Twitch application (provides the `TWITCH_CLIENT_ID`).
2. Maison credentials (username + password from `users.json`).
3. A `config/rewards.toml` mapping reward titles / chat commands to Maison actions.

### 1. Register a Twitch application

1. Open <https://dev.twitch.tv/console/apps> while logged in with the broadcaster account.
2. Click **Register Your Application**.
3. Fill the form:
   - **Name**: anything (e.g. `homie-home-bot`). Must be unique on Twitch globally.
   - **OAuth Redirect URLs**: Twitch requires at least one valid URL even though Device Code Flow never uses it. Put `http://localhost` and click "Add". That's enough.
   - **Category**: pick *Application Integration*.
   - **Client Type**: choose **Public**. **This is the critical setting** — Device Code Flow is only enabled for public clients. If you pick *Confidential* the bot will fail at `wait_for_code` with `invalid_client`.
4. Click **Create**. On the next screen, copy the **Client ID**. You do **not** need a client secret — the Device Code Flow does not use one. Don't generate or store one.

The bot will request these scopes on first run:

| Scope                          | What it allows                              |
|--------------------------------|---------------------------------------------|
| `channel:read:redemptions`     | Receive channel-point redemption events.    |
| `user:read:chat`               | Receive chat messages via EventSub.         |
| `user:write:chat`              | Send chat replies via Helix.                |
| `user:bot`                     | Required for chat reads/writes as the bot.  |
| `channel:bot`                  | Required to chat in the broadcaster's room. |

You authorise these scopes once at first run (see [Running the bot](#running-the-bot)).

### 2. Maison credentials

The bot calls `POST /api/auth/login` with the username/password you put in your `users.json` on the Maison side. The login returns a JWT in the `maison_session` cookie that the bot then sends as `Authorization: Bearer <jwt>` on every subsequent call.

If you don't have a hashed password yet:

```sh
cargo run --manifest-path /path/to/maison/backend/Cargo.toml --bin hash_password -- 'your-password'
```

Copy the resulting hash into `users.json` under `password_hash`. See the Maison README for the exact format.

### 3. `.env` file

```sh
cp .env.example .env
```

Then edit `.env`:

```dotenv
TWITCH_CLIENT_ID=abcdefghijklmnopqrstuvwxyz0123
TWITCH_BROADCASTER_LOGIN=your_channel_login_lowercase

MAISON_BASE_URL=http://192.168.1.10:3033
MAISON_USERNAME=leonard
MAISON_PASSWORD=the-password-you-hashed

# Optional
HOMIE_STATE_DIR=./.homie
REWARDS_FILE=config/rewards.toml
RUST_LOG=homie=info
```

> `TWITCH_BROADCASTER_LOGIN` is the channel login (lowercase, no spaces, what comes after `twitch.tv/`). Not the display name.

### 4. `config/rewards.toml`

```sh
cp config/rewards.example.toml config/rewards.toml
```

This file maps Twitch rewards / chat commands to Maison actions. Each entry has:

- `match`: either `{ chat_command = "!word" }` (chat command, no Twitch tier required) or `{ reward = "<exact title>" }` (channel-point redemption, requires Affiliate or Partner). The `reward` title must match **exactly** what is displayed on Twitch — case-sensitive, accents preserved.
- `action`: the Maison action to run (`kind` chooses the variant; see below).
- `reply` (optional): chat message sent on success. Omit it to echo the Maison response instead. Set it to `""` to suppress the reply entirely.
- `admin_only` (optional, defaults to `false`): when `true`, ignore the command unless the sender has the broadcaster or moderator badge.

### Built-in chat commands

These work without any entry in `rewards.toml`:

| Command            | Who           | What it does                                                                                                |
|--------------------|---------------|-------------------------------------------------------------------------------------------------------------|
| `!commands`        | anyone        | Lists every available command (built-ins + user rules), split between public and admin-only.                |
| `!yt <URL>`        | anyone        | Adds a track to the music queue. Rejects live streams and tracks longer than 10 minutes.                    |
| `!queue`           | anyone        | Shows what's playing and the next 5 tracks in the queue.                                                    |
| `!volume <0-100>`  | broadcaster/mod | Sets playback volume. Without an argument, replies with the current volume.                                |
| `!skip`            | broadcaster/mod | Skips the current track and starts the next one.                                                           |

When the first viewer track of a session starts, the bot calls `nowplaying-cli togglePlayPause`. That CLI talks to Apple's private `MediaRemote.framework` and operates on whichever app currently owns Now Playing — Apple Music, Spotify, a YouTube tab in any browser, Tidal, Apple Podcasts, anything. The bot then waits ~350 ms so the system audio buffer drains, then plays its own track. When the queue empties, it calls `togglePlayPause` again to resume the user's audio.

> Pure AppleScript media keys (`tell application "System Events" to key code 100`) **do not** trigger Now Playing on modern macOS — they send F8 as a regular keystroke. `nowplaying-cli` is the only reliable shell-level path to Now Playing, hence the prerequisite.

#### Action kinds

| `kind`             | Required fields                                    | Calls                                                       |
|--------------------|----------------------------------------------------|-------------------------------------------------------------|
| `lamp_power`       | `lamp_id`, `enabled`                               | `POST /api/zigbee/lamps/{id}/power`                          |
| `lamp_brightness`  | `lamp_id`, `brightness` (0-255)                    | `POST /api/zigbee/lamps/{id}/brightness`                     |
| `lamp_temperature` | `lamp_id`, `temperature`                           | `POST /api/zigbee/lamps/{id}/temperature`                    |
| `lamp_color`       | `lamp_id`, `x`, `y` (CIE xy)                       | `POST /api/zigbee/lamps/{id}/color`                          |
| `lamp_effect`      | `lamp_id`, `effect`                                | `POST /api/zigbee/lamps/{id}/effect`                         |
| `ac_mitsubishi`    | `host`, `command`, optional `model`, `local_ip`    | `POST /api/broadlink/mitsubishi/send`                        |
| `feeder_feed`      | `device_id`, optional `portion` (default `1`)      | `POST /api/devices/{id}/feeder/feed`                         |

#### Where to find the IDs

Once Maison is logged in (you can do it via the web UI or via curl with the cookie), these endpoints list everything you need:

```sh
# Zigbee lamps — copy the `id` field
curl -b cookie.jar http://192.168.1.10:3033/api/zigbee/lamps

# Tuya devices (feeder, fountain, litter box) — copy the `id` of the feeder
curl -b cookie.jar http://192.168.1.10:3033/api/devices

# Mitsubishi IR command names — `command` matches the `key` field
curl -b cookie.jar 'http://192.168.1.10:3033/api/broadlink/mitsubishi/codes?model=msz'
```

For the AC's `host`, that's the IP of the Broadlink IR blaster on your LAN, not the AC itself.

> **Lamp ID format**: Maison's native Zigbee backend accepts both the colon-separated EUI-64 form (`00:17:88:01:09:04:ff:17`) and the bare hex form (`00178801090400ff17`). The colon form is what `GET /api/zigbee/lamps` returns under `ieee_address`, and what the example config uses. The MQTT/Zigbee2MQTT backend uses whatever id Zigbee2MQTT exposes (often `0x...`); list `GET /api/zigbee/lamps` to be sure on your setup.

## Running the bot

```sh
cargo run --release
```

### First run

You will see something like:

```
────────────────────────────────────────────────────────
  Authorise homie by visiting:
    https://www.twitch.tv/activate
  Code: ABCD-EFGH
  (valid for 30 minutes)
────────────────────────────────────────────────────────
```

1. Open the URL in any browser logged in as the broadcaster account.
2. Enter the 8-character code.
3. Approve the scopes.
4. Back in the bot's terminal, the device flow polls Twitch and obtains a `UserToken`. The token is written to `./.homie/token.json` (configurable via `HOMIE_STATE_DIR`).
5. The bot then connects EventSub, registers two subscriptions (`channel.channel_points_custom_reward_redemption.add` and `channel.chat.message`), and starts dispatching.

### Subsequent runs

The bot reads the cached token. If still valid, it reuses it. If the access token expired (Twitch user tokens last ~4 hours), it refreshes it transparently using the stored refresh token. You only need to redo the device flow if the cache is deleted, the refresh token is revoked, or scopes change.

### Stopping the bot

`Ctrl+C` triggers a graceful shutdown.

## How it actually works

```
Twitch ----WebSocket----> homie ----HTTP Bearer----> Maison ----> hardware
        EventSub          (Rust)     /api/...           backend
```

- A single `tokio` task drives the WebSocket loop.
- On `session_welcome`, `homie` calls `helix::eventsub::CreateEventSubSubscriptionRequest` twice (once for redemptions, once for chat) using the freshly received `session_id`.
- On every `notification` frame, the matching `Event` enum variant is parsed by `twitch_api`, then dispatched to `actions::execute` which translates the rule's action variant into a `MaisonClient` method call.
- On `session_reconnect`, the loop closes the current socket and reconnects to the new URL Twitch supplied (no resubscribing needed — Twitch carries the subscriptions over).
- On any other socket error, the loop reconnects with capped exponential backoff (1s -> 2s -> 4s ... -> 60s).

Maison authentication is also opportunistic: the first request triggers a login, the JWT is reused via `Authorization: Bearer <jwt>` on every later call, and on a 401 response the client re-logs once and replays the request.

## Development

```sh
# Quality gate (run after every change)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Tests are entirely offline:

- `tests/config.rs` checks TOML deserialization of the example file.
- `tests/maison_client.rs` mocks the Maison API with `wiremock`, exercising the login flow, 401 retry, camelCase serialization, and the feeder/lamps payloads.
- `tests/rewards_dispatch.rs` feeds a documented EventSub redemption frame through `twitch_api::eventsub::Event::parse_websocket` and asserts the dispatcher resolves the right rule.

The full project conventions are documented in [`CLAUDE.md`](./CLAUDE.md).

## Troubleshooting

| Symptom                                                              | Likely cause                                                                                          |
|----------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------|
| `device code start failed: invalid_client`                           | Twitch app is *Confidential*. Recreate it as *Public*.                                                |
| `device code start failed: invalid_request` mentioning scopes        | Twitch revoked or doesn't recognize a scope. Check the scope list in [register a Twitch application](#1-register-a-twitch-application). |
| `Maison login failed: HTTP 401` on startup                           | Wrong `MAISON_USERNAME`/`MAISON_PASSWORD`, or the password is still plaintext in `users.json`.        |
| `request failed: HTTP 404` calling lamps                             | Wrong `lamp_id` in `rewards.toml`. List lamps with `GET /api/zigbee/lamps`.                            |
| `request failed: HTTP 400` on Mitsubishi send                        | `command` doesn't exist in the IR codes file. List with `GET /api/broadlink/mitsubishi/codes`.         |
| Bot connects but no events arrive after a redemption                 | Reward title in `rewards.toml` doesn't match the Twitch dashboard exactly (case, accents, whitespace). |
| `WebSocket recv error: Connection reset by peer` repeating           | Network blip. The bot reconnects automatically with backoff. If it persists, check outbound 443.       |
| Chat command does nothing                                            | `admin_only = true` and your account is missing the broadcaster/moderator badge.                       |
| `cached Twitch token invalid, falling back to device flow`           | Refresh token expired or scopes changed. Just walk through the device flow again.                      |

## License

MIT. Adapt as you see fit on your own home setup.
