use std::{ops::Deref, path::Path, str::FromStr};

use glob::Pattern;

use super::ResolvedLocation;
use crate::TraceEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: u8,
    pub creation_cycle: usize,
    pub ty: BreakpointType,
}

impl Default for Breakpoint {
    fn default() -> Self {
        Self {
            id: 0,
            creation_cycle: 0,
            ty: BreakpointType::Step,
        }
    }
}

impl Breakpoint {
    /// Create a new default `Breakpoint` of the given type
    pub fn new(ty: BreakpointType) -> Self {
        Self {
            ty,
            ..Default::default()
        }
    }

    /// Return the number of cycles this breakpoint indicates we should skip, or `None` if the
    /// number of cycles is context-specific, or the breakpoint is triggered by something other
    /// than cycle count.
    pub fn cycles_to_skip(&self, current_cycle: usize) -> Option<usize> {
        let cycles_passed = current_cycle - self.creation_cycle;
        match &self.ty {
            BreakpointType::Step => Some(1),
            BreakpointType::StepN(n) => Some(n.saturating_sub(cycles_passed)),
            BreakpointType::StepTo(to) if to >= &current_cycle => Some(to.abs_diff(current_cycle)),
            _ => None,
        }
    }
}
impl Deref for Breakpoint {
    type Target = BreakpointType;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.ty
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointType {
    /// Break at next cycle
    Step,
    /// Skip N cycles
    StepN(usize),
    /// Break at a given cycle
    StepTo(usize),
    /// Break at the first cycle of the next instruction
    Next,
    /// Break at the next source line, or the next instruction if no source location is available.
    NextLine,
    /// Break when we exit the current call frame
    Finish,
    /// Break when any cycle corresponds to a source location whose file matches PATTERN
    File(Pattern),
    /// Break when any cycle corresponds to a source location whose file matches PATTERN and occurs
    /// on LINE
    Line { pattern: Pattern, line: u32 },
    /// Break anytime the given operation occurs
    Opcode(OperationMatcher),
    /// Break when any cycle causes us to push a frame for PROCEDURE on the call stack
    Called(Pattern),
    /// Break when the given trace event occurs
    Trace(TraceEvent),
}
impl BreakpointType {
    /// Return true if this breakpoint indicates we should break for `current_op`
    pub fn should_break_for(&self, current_op: &miden_core::operations::Operation) -> bool {
        match self {
            Self::Opcode(matcher) => matcher.should_break_for(current_op),
            _ => false,
        }
    }

    /// Return true if this breakpoint indicates we should break on entry to `procedure`
    pub fn should_break_in(&self, procedure: &str) -> bool {
        match self {
            Self::Called(pattern) => pattern.matches(procedure),
            _ => false,
        }
    }

    /// Return true if this breakpoint indicates we should break at `loc`
    pub fn should_break_at(&self, loc: &ResolvedLocation) -> bool {
        match self {
            Self::File(pattern) => {
                pattern.matches_path(Path::new(loc.source_file.deref().content().uri().as_str()))
            }
            Self::Line { pattern, line } if line == &loc.line => {
                pattern.matches_path(Path::new(loc.source_file.deref().content().uri().as_str()))
            }
            _ => false,
        }
    }

    /// Returns true if this breakpoint is internal to the debugger (i.e. not creatable via :b)
    pub fn is_internal(&self) -> bool {
        matches!(
            self,
            BreakpointType::Next
                | BreakpointType::NextLine
                | BreakpointType::Step
                | BreakpointType::Finish
        )
    }

    /// Returns true if this breakpoint is removed upon being hit
    pub fn is_one_shot(&self) -> bool {
        matches!(
            self,
            BreakpointType::Next
                | BreakpointType::NextLine
                | BreakpointType::Finish
                | BreakpointType::Step
                | BreakpointType::StepN(_)
                | BreakpointType::StepTo(_)
        )
    }
}

impl FromStr for BreakpointType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();

        // b next
        // b finish
        // b after {n}
        // b for {opcode}
        // b at {cycle}
        // b in {procedure}
        // b {file}[:{line}]
        if s == "next" {
            return Ok(BreakpointType::Next);
        }
        if s == "finish" {
            return Ok(BreakpointType::Finish);
        }
        if let Some(n) = s.strip_prefix("after ") {
            let n = n.trim().parse::<usize>().map_err(|err| {
                format!("invalid breakpoint expression: could not parse cycle count: {err}")
            })?;
            return Ok(BreakpointType::StepN(n));
        }
        if let Some(opcode) = s.strip_prefix("for ") {
            return Ok(BreakpointType::Opcode(opcode.parse::<OperationMatcher>()?));
        }
        if let Some(cycle) = s.strip_prefix("at ") {
            let cycle = cycle.trim().parse::<usize>().map_err(|err| {
                format!("invalid breakpoint expression: could not parse cycle value: {err}")
            })?;
            return Ok(BreakpointType::StepTo(cycle));
        }
        if let Some(procedure) = s.strip_prefix("in ") {
            let pattern = Pattern::new(procedure.trim())
                .map_err(|err| format!("invalid breakpoint expression: bad pattern: {err}"))?;
            return Ok(BreakpointType::Called(pattern));
        }
        match s.split_once(':') {
            Some((file, line)) => {
                let pattern = Pattern::new(file.trim())
                    .map_err(|err| format!("invalid breakpoint expression: bad pattern: {err}"))?;
                let line = line.trim().parse::<u32>().map_err(|err| {
                    format!("invalid breakpoint expression: could not parse line: {err}")
                })?;
                Ok(BreakpointType::Line { pattern, line })
            }
            None => {
                let pattern = Pattern::new(s.trim())
                    .map_err(|err| format!("invalid breakpoint expression: bad pattern: {err}"))?;
                Ok(BreakpointType::File(pattern))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperationMatcher {
    Asm(String),
    Exact(miden_core::operations::Operation),
    Assert,
    Push,
    Dup,
    SwapW,
    Movup,
    Movdn,
}

impl OperationMatcher {
    pub fn should_break_for(&self, op: &miden_core::operations::Operation) -> bool {
        use miden_core::operations::Operation;
        match self {
            Self::Asm(_) => false,
            Self::Exact(expected) => op == expected,
            Self::Assert => matches!(op, Operation::Assert(_)),
            Self::Push => matches!(op, Operation::Push(_)),
            Self::Dup => matches!(
                op,
                Operation::Dup0
                    | Operation::Dup1
                    | Operation::Dup2
                    | Operation::Dup3
                    | Operation::Dup4
                    | Operation::Dup5
                    | Operation::Dup6
                    | Operation::Dup7
                    | Operation::Dup9
                    | Operation::Dup11
                    | Operation::Dup13
                    | Operation::Dup15
            ),
            Self::SwapW => matches!(op, Operation::SwapW | Operation::SwapW2 | Operation::SwapW3),
            Self::Movup => matches!(
                op,
                Operation::MovUp2
                    | Operation::MovUp3
                    | Operation::MovUp4
                    | Operation::MovUp5
                    | Operation::MovUp6
                    | Operation::MovUp7
                    | Operation::MovUp8
            ),
            Self::Movdn => matches!(
                op,
                Operation::MovDn2
                    | Operation::MovDn3
                    | Operation::MovDn4
                    | Operation::MovDn5
                    | Operation::MovDn6
                    | Operation::MovDn7
                    | Operation::MovDn8
            ),
        }
    }
}

impl core::fmt::Display for OperationMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Asm(op) => f.write_str(op),
            Self::Exact(op) => core::fmt::Display::fmt(op, f),
            Self::Assert => f.write_str("assert.*"),
            Self::Push => f.write_str("push.*"),
            Self::Dup => f.write_str("dup"),
            Self::SwapW => f.write_str("swapw"),
            Self::Movup => f.write_str("movup"),
            Self::Movdn => f.write_str("movdn"),
        }
    }
}

impl FromStr for OperationMatcher {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        use miden_core::operations::Operation::*;
        let opcode_parts = name
            .split_once('.')
            .map(|(name, rest)| (name, Some(rest)))
            .unwrap_or((name, None));
        let opcode = match opcode_parts {
            ("nop" | "noop", _) => Noop,
            ("assert", Some("*")) => return Ok(OperationMatcher::Assert),
            ("assert", Some(code)) => Assert(
                code.parse::<u32>()
                    .map(miden_core::Felt::from_u32)
                    .map_err(|err| err.to_string())?,
            ),
            ("assert", None) => Assert(miden_core::Felt::from_u32(0)),
            ("sdepth", None) => SDepth,
            ("caller", None) => Caller,
            ("clk", None) => Clk,
            ("emit", None) => Emit,
            ("add", None) => Add,
            ("neg", None) => Neg,
            ("mul", None) => Mul,
            ("inv", None) => Inv,
            ("incr", None) => Incr,
            ("and", None) => And,
            ("or", None) => Or,
            ("not", None) => Not,
            ("eq", None) => Eq,
            ("eqz", None) => Eqz,
            ("expacc", None) => Expacc,
            ("ext2mul", None) => Ext2Mul,
            ("u32split", None) => U32split,
            ("u32add", None) => U32add,
            ("u32assert2", Some(code)) => U32assert2(
                code.parse::<u32>()
                    .map(miden_core::Felt::from_u32)
                    .map_err(|err| err.to_string())?,
            ),
            ("u32assert2", None) => U32assert2(miden_core::Felt::from_u32(0)),
            ("u32add3", None) => U32add3,
            ("u32sub", None) => U32sub,
            ("u32mul", None) => U32mul,
            ("u32madd", None) => U32madd,
            ("u32div", None) => U32div,
            ("u32and", None) => U32and,
            ("u32xor", None) => U32xor,
            ("pad", None) => Pad,
            ("drop", None) => Drop,
            ("dup", _) => return Ok(OperationMatcher::Dup),
            ("swap", _) => Swap,
            ("swapw", _) => return Ok(OperationMatcher::SwapW),
            ("swapdw", _) => SwapDW,
            ("movup", _) => return Ok(OperationMatcher::Movup),
            ("movdn", _) => return Ok(OperationMatcher::Movdn),
            ("cswap", _) => CSwap,
            ("cswapw", _) => CSwapW,
            ("push", _) => return Ok(OperationMatcher::Push),
            ("advpop", _) => AdvPop,
            ("advpopw", _) => AdvPopW,
            ("mloadw", _) => MLoadW,
            ("mstorew", _) => MStoreW,
            ("mload", _) => MLoad,
            ("mstore", _) => MStore,
            ("mstream", _) => MStream,
            ("pipe", _) => Pipe,
            ("crypto_stream", _) => CryptoStream,
            ("hperm", _) => HPerm,
            ("mpverify", Some(code)) => MpVerify(
                code.parse::<u32>()
                    .map(miden_core::Felt::from_u32)
                    .map_err(|err| err.to_string())?,
            ),
            ("mpverify", None) => MpVerify(miden_core::Felt::from_u32(0)),
            ("mrupdate", None) => MrUpdate,
            ("frie2f4", None) => FriE2F4,
            ("horner_base", None) => HornerBase,
            ("horner_ext", None) => HornerExt,
            ("eval_circuit", None) => EvalCircuit,
            ("log_precompile", None) => LogPrecompile,
            _ => return Ok(OperationMatcher::Asm(name.to_string())),
        };

        Ok(OperationMatcher::Exact(opcode))
    }
}
