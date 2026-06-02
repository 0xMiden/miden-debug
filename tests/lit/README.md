# lit / FileCheck debugger tests

End-to-end tests that drive `miden-debug` over small Miden Assembly programs and
check its output with [`litcheck`](https://crates.io/crates/litcheck) (a
pure-Rust implementation of LLVM `lit` + `FileCheck`).

The debugger assembles the `.masm` programs itself, using the `miden-assembly` /
`miden-processor` version it is built against (the latest release the debugger
depends on, per `Cargo.toml`). No external compiler toolchain is required.

## Layout

```
tests/lit/
  lit.suite.toml     # suite config: only *.test files are test entry points
  add.masm           # a MASM program ...
  add.test           # ... and the test that debugs it
  breakpoint.masm
  breakpoint.test
  error.masm
  error.test
```

## How a test works

A `.test` file is, at once:

1. A **command script** for the debugger. `miden-debug --commands <file>` runs
   each non-`#` line as a REPL command (gdb's `-x` / lldb's `-s`). Lines starting
   with `#` are ignored by the debugger.
2. A **FileCheck input**. `filecheck` reads the `# CHECK:` lines and matches them
   against the debugger's output.
3. A **lit test**. The `# RUN:` line tells lit what to execute.

For example, `add.test`:

```text
# RUN: miden-debug --commands %s %S/add.masm 2>&1 | filecheck %s

continue
stack

# CHECK: Program terminated successfully
# CHECK: [0] 7
```

`%s` is the test file, `%S` its directory. The `RUN` line loads `add.masm` in the
debugger, runs the `continue` and `stack` commands, and pipes the output to
`filecheck`, which verifies the program finished and left `7` on top of the
operand stack.

## Running

```sh
cargo make test-lit
```

This builds `miden-debug` with the `repl` feature (needed for `--commands`),
installs it into `./bin`, and runs the suite via `litcheck lit run`.

## Adding a test

Drop a new `<name>.masm` program under `tests/lit/` and a `<name>.test` beside it
with the `RUN`/`CHECK` directives and debugger commands. Two MASM facts to keep
in mind:

- The operand stack starts as 16 padding zeros and is always shown padded.
- The VM requires at most 16 elements at program end — fold results into a
  padding zero (e.g. a trailing `add`) to terminate cleanly, or assert on the
  resulting error like `error.test`.

Prefer asserting on stable facts (a known computed result) over incidental
formatting.
