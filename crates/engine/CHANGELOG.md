# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/0xMiden/miden-debug/compare/miden-debug-engine-v0.6.1...miden-debug-engine-v0.7.0) - 2026-05-01

### Added

- implement more ergonomic testing primitives, update tests
- enable `println` via trace
- Add `TraceEvent::PrintLn`
- add vars command to display debug variables

### Fixed

- improve calltrace
- resolve source variables from frame base
- cleanup
- FrameBase resolution
- adapt debug variable tracking to v0.22 API

### Other

- fix formatting
- update dependencies
- output `println` content to `DebugLogger`
- change TRACE_PRINT_LN mnemonic
- cleanup log
- explain the selection of MAX_PRINTLN_BYTES
- limit length of message for `TRACE_PRINT_LN`
- don't panic in `decode_println`
- log errors in `capture_trace`
- Make `TraceHandler` take `ProcessorState` as arg
- fix formatting after rebase
- apply rustfmt formatting

## [0.6.1](https://github.com/0xMiden/miden-debug/releases/tag/miden-debug-engine-v0.6.1) - 2026-04-08

### Fixed

- broken workspace configuration
- VM v0.22.1 compatibility
- align deps with miden-client beta base
- handle edit-and-continue and terminate-and-reconnect
- pin protocol rev and align debugger DAP support
- resolve remote refresh, crate split, reload, and review follow-up

### Other

- fix linter warning
- release
- switch to crates.io deps for protocol and VM
- move files to avoid `#[path = "..."]`

## [0.1.0](https://github.com/0xMiden/miden-debug/releases/tag/miden-debug-engine-v0.1.0) - 2026-04-08

### Fixed

- VM v0.22.1 compatibility
- align deps with miden-client beta base
- handle edit-and-continue and terminate-and-reconnect
- pin protocol rev and align debugger DAP support
- resolve remote refresh, crate split, reload, and review follow-up

### Other

- switch to crates.io deps for protocol and VM
- move files to avoid `#[path = "..."]`
