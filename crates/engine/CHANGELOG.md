# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0](https://github.com/0xMiden/miden-debug/compare/miden-debug-engine-v0.8.1...miden-debug-engine-v0.9.0) - 2026-07-13

### Added

- *(prof)* collect and print op histogram
- *(engine)* write a replay snapshot even when the debugged run fails
- add `miden-debug --replay <snapshot>` to replay recorded sessions
- *(engine)* capture and serialize replay snapshots from DAP sessions
- *(engine)* expose event-mutation recording through DapConfig
- *(engine)* record advice mutations from event handlers for replay
- report missing package dependencies ([#91](https://github.com/0xMiden/miden-debug/pull/91))

### Fixed

- preserve replay snapshot state
- *(engine)* report the recorded event count correctly when writing a snapshot

### Other

- merge main back to next
- 0.9.0
- support externally-defined instruments
- configure `reports_dir` where all reports are written
- migrate to miden-vm v0.25
- Merge branch 'next' into fix/record
- apply nightly rustfmt
- *(engine)* make event recording an internal detail of the host
- *(engine)* move advice-mutation helpers into a support module

## [0.8.1](https://github.com/0xMiden/miden-debug/compare/miden-debug-engine-v0.8.0...miden-debug-engine-v0.8.1) - 2026-05-26

### Fixed

- read unwritten rust memory as zero
- `read_from_rust_memory`
- resolve relative source paths from debugger

## [0.8.0](https://github.com/0xMiden/miden-debug/compare/miden-debug-engine-v0.7.1...miden-debug-engine-v0.8.0) - 2026-05-20

### Added

- *(dap)* support standalone debug adapter launch
- *(dap)* handle Command::Attach for VS Code DAP clients
- extend memory queries to DebugExecutor
- re-export crates whose types appear in our public api
- expose some basic helpers from DebugExecutor

### Fixed

- *(dap)* preserve events while waiting for responses
- fix clippy
- *(dap)* track source variables accurately
- *(dap)* re-emit Initialized after attach/launch for clients that miss the first one
- *(dap)* announce thread 1 before initial Stopped event
- *(dap)* support clients that skip configuration

### Other

- address PR comment
- address PR comments
- bump miden-vm to 0.23 / miden-crypto to 0.25
- simplify opcode breakpoints

## [0.7.1](https://github.com/0xMiden/miden-debug/compare/miden-debug-engine-v0.7.0...miden-debug-engine-v0.7.1) - 2026-05-06

### Fixed

- remove miden-tx/miden-protocol dependencies
- separate versioning of miden-debug and its subcrates

### Other

- merge main into next

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
