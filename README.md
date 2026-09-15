# Miden Debugger

This repo provides the implementation of the `miden debug` command, i.e. an interactive debugger for Miden programs.

The underlying `miden-debug` crate may also be used as a library, for use cases where you want to use the debugger as an executor for Miden programs, such as
in tests, etc.

See the [documentation](https://github.com/0xMiden/compiler/tree/next/docs/external/src/guides/debugger.md) for more details on the `miden debug` command, and how to use the debugger.

## Engine features

`miden-debug-engine` uses `#![no_std]` and `alloc`. Without default features, it provides
in-memory package decoding, typed variable resolution, memory inspection, input parsing, and
replay serialization. The default `std` feature adds interactive execution, profiling, filesystem
access, and CLI parsers; `dap` also enables `std`.

The engine is checked and tested separately with `--no-default-features`. Fully bare-metal builds
still require upstream dependency fixes: VM 0.30 pulls in std-only dependencies such as `flume`
through `miden-crypto` and `textwrap` through `miden-miette/fancy-no-syscall`.

## Coverage

Run the workspace tests with LLVM source coverage locally with:

```bash
cargo make coverage
```

This writes a Cobertura report to `rust-coverage.xml`. CI uploads the same report as the
`rust-coverage` artifact and requires at least 80% line coverage for the debugger core. The
terminal event loop, DAP transport glue, and test-only support binaries are excluded from this
gate; TUI panes are covered separately with Ratatui's in-memory test backend.

## License

MIT
