# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Fixed

- Stop displaying stale variables after successful program termination in the REPL, TUI, and
  scripting API, while preserving variable inspection after execution errors ([#90](https://github.com/0xMiden/miden-debug/issues/90)).

## [0.14.0]

A trivial version bump to unify the versions of the various debugger crates. You can find the legacy changelog entries in [CHANGELOG-legacy.md].

### Changed

- The `miden-debug-engine` and `miden-debug-dap` crates now release at the same version as `miden-debug` itself.
- Require compiled `.masp` package artifacts for debugger inputs and linked libraries; source and
  project compilation must be performed by `midenc` or `miden build` before debugging.

### Fixed

- Report incompatible package and debug-info formats with guidance to use the matching midenup toolchain.
- Make the engine's `no_std` boundary explicit, keeping portable package and value handling available without `std` and gating interactive execution and filesystem access behind `std`.
