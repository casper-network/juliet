# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

* There is now a timeout for how long a peer can take to accept an error message.
* Application errors (`ErrorKind::OTHER`) are now truncated to fit into a single frame.

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
