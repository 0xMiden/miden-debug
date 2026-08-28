# lit / FileCheck debugger tests

End-to-end tests that drive `miden-debug` over small Miden Assembly programs and
check its output with [`litcheck`](https://crates.io/crates/litcheck) (a
pure-Rust implementation of LLVM `lit` + `FileCheck`).

The debugger only loads compiled packages. The lit harness builds the local
`compile-masm` helper and uses it to assemble each `.masm` fixture into a
temporary `.masp` package before running `miden-debug`. Tests that need typed
debug metadata use `compile-abi-fixture` to add deterministic ABI types and
variable locations to the parsed AST; the assembler encodes them in the package.

When a package entrypoint carries a component-model signature, trailing CLI arguments are encoded
with that signature's canonical ABI and the completed result is decoded with the same metadata.
This lets Rust values such as `u64` use their multi-felt representation while untyped MASM
programs retain raw-felt argument handling.
Typed frame-base and expression locations are read from packed byte-addressed memory before being
lifted into that canonical representation, including values that cross Miden element boundaries.

Python scripting tests run `miden-debug-python`, a second local copy built with
the `python` feature, and pass `--no-user-python-init` so user configuration
cannot affect their output.

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
# RUN: compile-masm %S/add.masm -o %t.masp
# RUN: miden-debug --commands %s %t.masp 2>&1 | filecheck %s

continue
stack

# CHECK: Program terminated successfully
# CHECK: [0] 7
```

`%s` is the test file, `%S` its directory, and `%t` is a per-test temporary file.
The first `RUN` line compiles `add.masm`; the second loads the resulting program
in the debugger, runs the `continue` and `stack` commands, and pipes the output
to `filecheck`, which verifies the program finished and left `7` on top of the
operand stack.

## Running

```sh
cargo make test-lit
```

This installs the lit tools, builds `miden-debug` with the `repl` feature and a
second copy with the `python` feature, builds the local MASM-to-package helper,
installs the binaries into `./bin`, and runs the suite via `litcheck lit run`.

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

For stack-trace tests, `compile-masm` accepts repeated
`--inline-call <name,line,column>` options. They attach an innermost-first
inline call chain to the fixture's operations without requiring `midenc` in the
debugger CI job:

```text
compile-masm inline_call.masm -o inline_call.masp \
  --inline-call fixture::inner,3,1 \
  --inline-call fixture::outer,2,1
```
