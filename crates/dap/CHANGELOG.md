# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0](https://github.com/0xMiden/miden-debug/compare/miden-debug-dap-v0.7.1...miden-debug-dap-v0.8.0) - 2026-05-20

### Added

- *(dap)* support standalone debug adapter launch

### Fixed

- *(dap)* track source variables accurately
- *(dap)* support clients that skip configuration

### Other

- 0.8.0
- merge next into feature/flamegraph
- address PR comments
- bump miden-vm to 0.23 / miden-crypto to 0.25

## [0.7.1](https://github.com/0xMiden/miden-debug/compare/miden-debug-dap-v0.7.0...miden-debug-dap-v0.7.1) - 2026-05-06

### Fixed

- remove miden-tx/miden-protocol dependencies
- separate versioning of miden-debug and its subcrates

### Other

- merge main into next

## [0.7.0](https://github.com/0xMiden/miden-debug/compare/miden-debug-dap-v0.6.1...miden-debug-dap-v0.7.0) - 2026-05-01

### Other

- update dependencies

## [0.6.1](https://github.com/0xMiden/miden-debug/compare/miden-debug-dap-v0.1.0...miden-debug-dap-v0.6.1) - 2026-04-08

### Fixed

- broken workspace configuration

## [0.1.0](https://github.com/0xMiden/miden-debug/releases/tag/miden-debug-dap-v0.1.0) - 2026-04-08

### Added

- add transaction DAP debugging support

### Fixed

- event-driven remote UI refresh via custom miden/uiState DAP event
- address comments
- adopt to latest changes in protocol
