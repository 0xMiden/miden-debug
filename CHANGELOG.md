# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Fixed

- Unqualified function breakpoints now consistently match fully-qualified procedure-name suffixes in local and DAP sessions ([#88](https://github.com/0xMiden/miden-debug/issues/88)).

## [0.15.0]

### Added

- Incompatible package and debug-info format errors now explain how to select the producing toolchain with `miden +<toolchain> debug <FILE>`.

### Migration and breaking changes

- Debugger inputs and linked libraries must now be compiled `.masp` packages, including when starting a DAP server or generating a flamegraph. Compile sources and projects with `midenc` or `miden build` first, and replace source paths, project directories, and `-l masm=...` arguments with compiled package paths.
- Rust scripting integrations must replace `ScriptDebugger::from_masm_source` with `ScriptDebugger::from_config`, supplying a compiled package through `DebuggerConfig.input`. Calls to `LinkLibrary::load` must drop the registry argument and pass only the search paths.
- The debugger now uses Miden VM 0.32.1 instead of 0.30.0. Rust integrations exchanging VM, package, or debug-info types with the debugger must use matching 0.32.1 dependencies.
- Builds with default features disabled must enable `std` for filesystem package loading, CLI parsers, and automatic profiling report output. The engine retains in-memory package decoding, variable and memory inspection, input parsing, and replay serialization without `std`; enabling `dap` or `proptest` now also enables `std`.
- `InputFile` is now a struct: replace `InputFile::Real(path)` with `InputFile::from_path(path)` and `InputFile::Stdin(bytes)` with `InputFile::new("stdin://", Some(bytes))`. Use `uri()` or `to_path()` instead of matching enum variants, and handle the `Result` returned by `bytes()` instead of an `Option`.
- Rust callers constructing `BreakpointType::File`, `Line`, or `Called` must replace `glob::Pattern` with `miden_debug_engine::glob::Glob::new(pattern)?.compile_matcher()`. Existing breakpoint patterns also need checking: URI schemes are stripped before matching, braces denote alternatives, and backslashes escape characters on Unix. Use `[{]` and `[}]` to match literal braces.
- `CallStack::new` now takes an event map wrapped in `Arc<miden_utils_sync::RwLock<_>>` instead of `Arc<std::sync::Mutex<_>>`; update callers and their event-map locking accordingly.
- Custom profiling instruments must change `Instrument::write_report_to` to accept `&mut dyn profiling::OutputWriter` and return `profiling::OutputResult<()>`. The `path` fields in `ReplaySnapshotWrite` and `ReplaySnapshotWriteError` now use `debug_types::Uri` instead of `PathBuf`; convert paths when constructing or reading these records.
- `LinkLibrary.linkage` no longer accepts `miden_assembly::Linkage`, it uses its own type, but is currently unused, and exists only as a placeholder for if/when linkage becomes relevant to the debugger.
- Function breakpoints now stop after entry-block variable declarations have been recorded, so entry variables are available for inspection in local and DAP sessions. Scripts relying on the previous stop cycle must account for the later stop. Rust integrations using `DebugExecutor::procedure_has_debug_vars` must adapt to `should_wait_for_entry_variables`, which checks pending entry declarations rather than whether a procedure has any debug variables ([#89](https://github.com/0xMiden/miden-debug/issues/89)).
- After successful termination, the REPL and TUI no longer display stale variables, and scripting variable queries return an empty list. Inspect variables at a breakpoint before completion; final stack outputs and typed results remain available, as do variables after execution errors ([#90](https://github.com/0xMiden/miden-debug/issues/90)).
- Package loading now validates debug information immediately and rejects malformed or incompatible debug-info sections. Rebuild affected artifacts, or select their producing toolchain for format mismatches.

## [0.14.0]

A trivial version bump to unify the versions of the various debugger crates. You can find the legacy changelog entries in [CHANGELOG-legacy.md].

### Changed

- The `miden-debug-engine` and `miden-debug-dap` crates now release at the same version as `miden-debug` itself.
