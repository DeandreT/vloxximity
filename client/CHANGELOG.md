# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/DeandreT/vloxximity/releases/tag/vloxximity-client-v0.1.0) - 2026-05-01

### Added

- configurable speaking indicator overlay
- auto-join squad/party rooms
- add room controls and per-type ptt
- add group-aware multi-room voice routing
- wss + os keyring for api key. harden server with size + rate limits. cleanup dead webrtc scaffolding
- persist settings + per-account mutes. require gw2 api key to join
- add 3d spatial audio

### Fixed

- explicit lifetime on log_channel return
- collapse nexus log channels outside debug mode
- persist gw2 api key under wine/proton
- parse mumble context when gw2 reports short context lengths
- direction audio swapped.
- weirdchamp

### Other

- replace release-please with release-plz
- use literal versions in member crates
- run cargo fmt
- use rtapi group type for room suggestions
- Rename keybinds
- first commit
