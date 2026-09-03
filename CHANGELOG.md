# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.14.0]

A trivial version bump to unify the versions of the various debugger crates. You can find the legacy changelog entries in [CHANGELOG-legacy.md].

### Changed

- The `miden-debug-engine` and `miden-debug-dap` crates now release at the same version as `miden-debug` itself.

### Fixed

- Report incompatible package and debug-info formats with guidance to use the matching midenup
  toolchain.
