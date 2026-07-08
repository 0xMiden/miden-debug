---
title: Profiling
sidebar_position: 10
---

# Profiling

Profiling generates an execution profile that shows which operations consume the most cycles. This helps identify hotspots in Miden Assembly programs.

## Usage

Run the debugger with `--commands` to execute a script and `--profile-op-histogram-out` to collect and write the profile:

```bash
miden-debug \
    --profile-op-histogram-out ./op_histogram.txt \
    --commands cmds.txt \
    fibonacci:fibonacci.masp -- 42
```

The `cmds.txt` script should contain:

```text
# Run until completion
continue
# Optionally print stack outputs
stack
```

Once passing commands as CLI parameters is supported [[#97](https://github.com/0xMiden/miden-debug/issues/97)], no separate `cmds.txt` file will be needed.

## Interpretation

The output file contains a histogram of operation counts sorted by frequency. Counts are weighted by cycle, so if `opx` takes 4 cycles and is executed twice, its value in the histogram will be 8.
