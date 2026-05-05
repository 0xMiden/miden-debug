---
title: REPL mode
sidebar_position: 4
---

# REPL mode

REPL mode swaps the full-screen TUI for a plain readline prompt — the same
execution engine, breakpoint model, and program-state inspection, but driven
by typed commands and rendered as line-oriented output. It's the right mode
when:

- the terminal is a slow remote tty,
- you're scripting the debugger or recording sessions,
- ANSI / UTF-8 features aren't available, or
- you just prefer line-by-line output.

```bash
miden-debug --repl sum.masm
```

Every TUI capability is reachable via REPL commands; the differences are
cosmetic. Breakpoint expressions accepted by the REPL match exactly those
accepted by the TUI's `:break` prompt.

## Command reference

Each command line starts with one of these. Brackets denote optional
arguments.

### Execution

| Command | Aliases | Effect |
| ------- | ------- | ------ |
| `step [N]` | `s` | Execute one VM cycle, or `N` cycles if given |
| `next` | `n` | Execute until the next instruction boundary |
| `next-line` | `nl`, `nextline` | Execute until the next source line |
| `continue` | `c` | Run until a breakpoint or program end |
| `finish` | `e` | Run until the current call frame returns |
| `reload` | | Restart the program from disk |

### Breakpoints

| Command | Aliases | Effect |
| ------- | ------- | ------ |
| `break <SPEC>` | `b`, `breakpoint` | Create a breakpoint — see [Breakpoint specs](#breakpoint-specs) |
| `breakpoints` | `bp` | List active breakpoints |
| `delete [ID]` | `d` | Delete one breakpoint by id, or all breakpoints when `ID` is omitted |

### Inspection

| Command | Aliases | Effect |
| ------- | ------- | ------ |
| `stack` | | Print the operand stack |
| `mem ADDR [OPTS]` | `memory` | Read linear memory — same syntax as the TUI's `:r` |
| `locals` | | Print the procedure's locals (raw frame slots) |
| `vars [all]` | `variables` | Print source-level variables. `all` includes compiler-generated locals (named `local0`, `local1`, …) |
| `where` | `w` | Print the current source location and procedure |
| `list` | `l` | Print recently executed instructions |
| `backtrace` | `bt` | Print the call stack |

### Other

| Command | Aliases | Effect |
| ------- | ------- | ------ |
| `help` | `h`, `?` | Show the inline help |
| `quit` | `q`, `exit` | Exit |

## Breakpoint specs

Identical to the TUI prompt. `<SPEC>` is one of:

| Spec | Meaning |
| ---- | ------- |
| `<FILE>[:LINE]` | Glob match against the source-file path; optionally restricted to `LINE` |
| `in <PATTERN>` | Glob match against the fully-qualified procedure name on entry |
| `for <OPCODE>` | Match a literal opcode (with immediates) |
| `next` | Break on the next instruction boundary (one-shot) |
| `after <N>` | Break after `N` more cycles (one-shot) |
| `at <CYCLE>` | Break when the cycle counter reaches `CYCLE` (one-shot, no-op if past) |
| `finish` | Break when the current call frame returns (one-shot) |

Examples:

```text
b sum.masm:5            # line breakpoint
b in std::math::*       # any procedure under std::math
b for swap              # any swap instruction
b after 1000            # 1000 more cycles
b at 50000              # cycle 50000
b finish                # exit current frame
```

## Memory expressions

Same grammar as the TUI's `:r`. Examples:

```text
mem 0x1000
mem 0x1000 -t felt
mem 0x1000 -t u32 -c 4 -f x
mem 1024 -m bytes -c 16
```

See the TUI guide's [Reading memory](./tui.md#reading-memory) section for the
full option matrix.

## Variables

`vars` reports source-level variables resolved from DWARF info. Compiler
locals (`local0`, `local1`, …) are hidden by default; pass `all` to include
them:

```text
vars         # only DWARF-named variables visible at the current line
vars all     # everything, including compiler temporaries
```

Each entry is `name=value` when the storage is materialised, otherwise
`name=<location-spec>`.

## Example session

```text
$ miden-debug --repl sum.masm
(miden-debug) b sum.masm:3
breakpoint 1 set
(miden-debug) c
hit breakpoint 1 at sum.masm:3
(miden-debug) stack
[1, 2]
(miden-debug) s
(miden-debug) stack
[3]
(miden-debug) c
program terminated
(miden-debug) q
```

## Tips

- Up/down arrows recall previous commands (rustyline history).
- `help` (or `h`, `?`) prints the same table you've just read; handy when you
  want a refresher without leaving the prompt.
- `MIDENC_TRACE=miden_debug=debug miden-debug --repl ...` emits the
  debugger's own log to stderr — useful when an apparent bug might be
  misuse, an inputs-file error, or a missing source file.
