---
title: The Miden Debugger
sidebar_position: 1
---

# The Miden Debugger

`miden-debug` is the interactive debugger for Miden VM programs. It loads a
compiled `.masp` (Miden Assembly Package) or a `.masm` source, sets up the
Miden VM, and lets you step through execution at the cycle, instruction, or
source-line level — with breakpoints, memory inspection, stack views, and
source-level variable resolution from DWARF info.

The same crate ships three execution modes:

| Mode | Flag | When to use |
| ---- | ---- | ----------- |
| **TUI** (default) | _none_ | Full-screen interactive UI with live source, disassembly, operand stack, call stack, and breakpoints. See [TUI mode](./tui.md). |
| **REPL** | `--repl` | Plain-text interactive prompt — useful over slow terminals, for scripting, or when ANSI/UTF-8 features aren't available. **Opt-in:** rebuild with `--features repl`. See [REPL mode](./repl.md). |
| **DAP client** | `--dap-connect ADDR` | Connect to a remote DAP server (typically `miden-client exec --start-debug-adapter`) and drive it through the same TUI. See [DAP](./dap.md). |

The TUI and REPL share the same execution engine and breakpoint model — the
difference is purely how the UI is rendered and how you type commands.

## Installation

```bash
git clone https://github.com/0xMiden/miden-debug
cd miden-debug
cargo build --release --bin miden-debug
./target/release/miden-debug --version
```

The default build enables the `tui` and `dap` features. **REPL mode is opt-in**
— rebuild with `--features repl` to use `--repl`:

```bash
cargo build --release --features repl --bin miden-debug
```

To strip everything except what you need, combine `--no-default-features` with
the explicit feature list, for example `--features tui,repl`.

## Quick start

```bash
# Compile a program (or use any existing .masp / .masm)
cat > sum.masm <<'EOF'
begin
    push.1
    push.2
    add
end
EOF

# Drop into the TUI
miden-debug sum.masm

# Or the plain REPL
miden-debug --repl sum.masm

# Or attach to a running miden-client DAP server
miden-debug --dap-connect 127.0.0.1:4711
```

When the debugger starts on a freshly-loaded program, it stops at cycle 0 so
you can set breakpoints before execution begins.

## Where to go next

- **[CLI reference](./cli.md)** — every flag accepted by `miden-debug`, including
  inputs, linker, working directory, and entrypoint selection.
- **[TUI mode](./tui.md)** — keyboard shortcuts, panes, the `:` command prompt,
  breakpoints, reading memory, and `:vars` for source variables.
- **[REPL mode](./repl.md)** — text-only command reference for the same
  capabilities, organised for quick lookup.
- **[DAP integration](./dap.md)** — how the debugger wires up to `miden-client`
  for transaction-script debugging, the architecture of the DAP server,
  and how IDE clients (VS Code, Zed, the TUI itself) attach to it.
