# Usage

## Running the Debugger

```bash
# TUI mode (default)
miden-debug program.masp

# REPL mode
miden-debug --repl program.masp
miden-debug -r program.masp
```

## REPL Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `step [N]` | `s` | Step N cycles (default 1) |
| `next` | `n` | Step to next instruction |
| `continue` | `c` | Run until breakpoint/end |
| `finish` | `e` | Run until function returns |
| `break <spec>` | `b` | Set breakpoint |
| `breakpoints` | `bp` | List breakpoints |
| `delete [id]` | `d` | Delete breakpoint(s) |
| `stack` | | Show operand stack |
| `mem <addr>` | | Show memory |
| `vars` | | Show debug variables |
| `where` | `w` | Show current location |
| `list` | `l` | Show recent instructions |
| `backtrace` | `bt` | Show call stack |
| `reload` | | Restart program |
| `help` | `h` | Show help |
| `quit` | `q` | Exit |

## Breakpoint Specs

```
break at 100       # at cycle 100
break after 50     # after 50 cycles
break in foo       # when entering procedure foo
break file.masm:10 # at file:line
```
