# Integration tests

These are tests which require compilation into separate binaries, so as to avoid conflicts related to global state within the debugger (e.g. the global logger).

## Test-specific notes

The following sections document why specific tests are handled here, rather than as plain unit tests.

### Testing the behavior of the `println` trace event

Related to all tests starting with `println_`.

Printed lines are displayed in `DebugLogger`, which is built on `env_logger`. The logger initialized by `env_logger` is global per process. To let each `println` test have its own process-local `DebugLogger`, we create a separate file here for each `println` test. The way integration tests are built and run ensures that this gives each `println` test its own `DebugLogger`. Otherwise multiple tests would concurrently write to the same `DebugLogger`.

`DebugLogger` is feature-gated behind `tui`. To apply that gate to integration tests, add an entry according to the following pattern for each relevant file:

```toml
[[test]]
name = "println_smoke_test"
required-features = ["tui"]
```

## Scriptable REPL harness (`repl_harness`)

`repl_harness.rs` runs every `tests/repl/*.repl` file as a debugger test. Each
file embeds an inline Miden Assembly program and a script of REPL commands
annotated with lit/FileCheck-style assertions:

```text
begin
    push.3 push.4 add add
end
---
continue
# CHECK: Program terminated successfully
stack
# CHECK: Operand Stack
# CHECK: [0] 7
# CHECK-NOT: Error
```

The two sections are separated by a line containing only `---`:

1. **Program** — inline MASM, assembled on the fly (no `.masp` package needed).
2. **Script** — REPL commands interleaved with `#` directives.

The commands are executed via `miden_debug::run_script`, producing a transcript
(plain-text prompts + echoed commands + command output). The directives are then
matched against that transcript in order:

- `# CHECK: <text>` — `<text>` appears on some line at or after the previous
  match (substring match).
- `# CHECK-NEXT: <text>` — `<text>` appears on the immediately following line.
- `# CHECK-NOT: <text>` — `<text>` does not appear before the next positive
  match.

`#` lines that are not directives are comments. Each `CHECK` consumes through the
end of the line it matches, so two `CHECK`s cannot match the same line.

Run it (requires the `repl` feature):

```sh
cargo test --features repl --test repl_harness
```

Set `REPL_DUMP=1` to print every transcript, which is handy when authoring a new
`.repl` file or when an assertion fails:

```sh
REPL_DUMP=1 cargo test --features repl --test repl_harness -- --nocapture
```

Two MASM gotchas when writing programs: the operand stack starts as 16 padding
zeros and is always shown padded, and the VM requires at most 16 elements at
program end — fold results into a padding zero (e.g. a trailing `add`) to
terminate cleanly, or assert on the resulting error like `tests/repl/error.repl`.
