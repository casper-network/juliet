# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

* A bug that caused connections seeing a high incidence of multi-frame sends to collapse due to a protocol violation by the sender has been fixed.

## [0.2.1] - 2023-01-23

### Changed

* There is now a timeout for how long a peer can take to accept an error message.
* Application errors (`ErrorKind::OTHER`) are now truncated to fit into a single frame.

### Fixed

* The IO layer will no longer drop frames if no multi-frame payloads are sent while a non-multi-frame payload has been moved to the wait queue due to exceeding the in-flight request limit.
* The outgoing request queue will now process much faster in some cases when filled with large numbers of requests.
* The `io` layer will no longer attempt to allocate incredibly large amounts of memory under certain error conditions.

## [0.2.0] - 2023-11-24

### Changed

* The `Display` output format for `Header` has changed, instead of displaying the hex encoded raw header value, it now display human readable output.

### Removed

* `juliet` no longer depends on the `hex_fmt` crate.

## [0.1.2] - 2023-11-24

### Added

* A changelog has been added in `CHANGELOG.md`.
* `shell.nix` is now available to facilitate the same environment locally and CI.
* Added GitHub actions based CI.

## [0.1.1] - 2023-11-23

### Added

* Missing repository link in `Cargo.toml`

## [0.1.0] - 2023-11-23

Initial release.
