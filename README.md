# Miden Debugger

This repo provides the implementation of the `miden debug` command, i.e. an interactive debugger for Miden programs.

The underlying `miden-debug` crate may also be used as a library, for use cases where you want to use the debugger as an executor for Miden programs, such as
in tests, etc.

See the [documentation](https://github.com/0xMiden/compiler/tree/next/docs/external/src/guides/debugger.md) for more details on the `miden debug` command, and how to use the debugger.

## Transaction Debugging Stack

The transaction debugging work on `pr/enable-tx-debugging` is a stacked integration:

- This branch depends on the debugger VM port work from issue `#30` / `pr/migrate-to-vm-v0.21`.
- The executor-factory abstraction is intended to live in the protocol/transaction layer
  (`miden-protocol` / `miden-tx`), not as a long-term VM or `miden-debug` API.
- The remaining PR order is protocol + `miden-tx` first, then `miden-client` once that hook lands.

## License

MIT
