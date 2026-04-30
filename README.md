# Vloxximity

Vloxximity is a Guild Wars 2 proximity voice chat addon for Nexus. It pairs a Nexus addon client with a lightweight Rust relay server so players can talk to nearby players, squad members, and party members with positional audio.

## Features

- Nexus addon client for Guild Wars 2.
- Map-based proximity rooms driven by MumbleLink position data.
- Squad and party voice rooms driven by Nexus RTAPI group events.
- Push-to-talk, voice activity, and always-on voice modes.
- Per-room-type PTT bindings for map, squad, and party chat.
- Directional and 3D spatial audio with distance attenuation.
- Input/output device selection, volume controls, mute, and deafen.
- Optional peer markers rendered at world-space peer positions.
- GW2 API key validation for account-aware identity and persistent mutes.
- WebSocket relay server with TLS support, rate limits, size limits, and idle cleanup.

## Repository Layout

```text
.
├── client/   # Nexus addon DLL crate
├── server/   # WebSocket relay server crate
└── Cargo.toml
```

The workspace contains two crates:

- `vloxximity-client`: compiled as a `cdylib` and loaded by Nexus.
- `vloxximity-server`: relay server for room membership, positions, and audio frames.

## Requirements

- Rust stable.
- Guild Wars 2 with Nexus installed for client usage.
- `cargo-xwin` and the MSVC Windows target for Linux cross-compilation of the client.
- A reachable relay server. The default client URL is `ws://localhost:8080/ws`.

Install the Windows build tools used by the client crate:

```sh
cargo install cargo-xwin
rustup target add x86_64-pc-windows-msvc
```

## Build

### Server

```sh
cargo build -p vloxximity-server --release
```

Run locally:

```sh
cargo run -p vloxximity-server
```

The server listens on `0.0.0.0:8080` and exposes:

- `GET /health`
- `GET /ws`

### Client

The client must be built for Windows. Native Linux `cargo check`/`cargo build` will fail in Windows-only dependencies.

```sh
cargo xwin build -p vloxximity-client --target x86_64-pc-windows-msvc --release
```

Output:

```text
target/x86_64-pc-windows-msvc/release/vloxximity_client.dll
```

## Install Client

Copy the built DLL into your Nexus addon directory.

```text
vloxximity_client.dll
```

Inside Nexus/GW2:

1. Open the Vloxximity settings window.
2. Set the relay server URL if you are not using `ws://localhost:8080/ws`.
3. Select input and output devices.
4. Bind push-to-talk keys in Nexus keybind settings.
5. Optionally add a GW2 API key with `account` permission for account validation and persistent mutes.

## Server Configuration

By default the server runs without TLS for local development.

For production, set both TLS environment variables:

```sh
export VLOXXIMITY_TLS_CERT=/path/to/fullchain.pem
export VLOXXIMITY_TLS_KEY=/path/to/privkey.pem
cargo run -p vloxximity-server --release
```

When TLS is enabled, clients should use a `wss://.../ws` server URL.

### Test Peers

The server can spawn synthetic peers for local audio/spatial testing:

```sh
cargo run -p vloxximity-server -- --testpeer
cargo run -p vloxximity-server -- --testpeer=orbit
cargo run -p vloxximity-server -- --testpeer=grid
```

`orbit` creates a moving peer. `grid` creates anchored peers at several distances.

## Client Settings

Vloxximity stores user settings in the Nexus-provided addon directory.

- `settings.json`: audio, room, spatial, and server settings.
- `mutes.json`: persisted account mute list.
- GW2 API key: Windows Credential Manager on native Windows.
- `api_key.txt`: Wine/Proton fallback.

Only the GW2 `account` permission is needed currently.

## Room Model

Room ids use a client-agreed prefix format:

```text
map:<map-or-instance-key>
squad:<server-cluster-id>
party:<server-cluster-id>
```

The relay treats room ids as opaque strings. The client uses the prefixes for routing, volume, spatial behavior, and PTT selection.

## Development

Format the workspace:

```sh
cargo fmt
```

Check the server crate:

```sh
cargo check -p vloxximity-server
```

Check or build the client through the Windows target path:

```sh
cargo xwin build -p vloxximity-client --target x86_64-pc-windows-msvc
```

## Notes

- Map rooms are managed automatically from MumbleLink.
- Squad and party rooms can auto-join when Nexus RTAPI reports group membership.
- Release builds collapse Nexus log output into one `Vloxximity` channel; debug builds keep per-module log channels.
