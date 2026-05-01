# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/DeandreT/vloxximity/releases/tag/vloxximity-server-v0.1.0) - 2026-05-01

### Added

- add group-aware multi-room voice routing
- wss + os keyring for api key. harden server with size + rate limits. cleanup dead webrtc scaffolding
- persist settings + per-account mutes. require gw2 api key to join

### Other

- replace release-please with release-plz
- use literal versions in member crates
- run cargo fmt
- first commit
