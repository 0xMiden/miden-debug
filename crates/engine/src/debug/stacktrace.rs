use std::{
    borrow::Cow,
    cell::{OnceCell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use miden_core::operations::AssemblyOp;
use miden_debug_types::{Location, SourceFile, SourceManager, SourceManagerExt, SourceSpan, Uri};
use miden_processor::{ContextId, operation::Operation, trace::RowIndex};

use crate::exec::TraceEvent;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ControlFlowOp {
    Span,
    Respan,
    Join,
    Split,
    End,
}

pub struct StepInfo<'a> {
    pub op: Option<Operation>,
    pub control: Option<ControlFlowOp>,
    pub asmop: Option<&'a AssemblyOp>,
    pub clk: RowIndex,
    pub ctx: ContextId,
}

#[derive(Debug, Clone)]
struct SpanContext {
    frame_index: usize,
    location: Option<Location>,
}

pub struct CallStack {
    trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>>,
    contexts: BTreeSet<Rc<str>>,
    frames: Vec<CallFrame>,
    block_stack: Vec<Option<SpanContext>>,
}
impl CallStack {
    pub fn new(trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>>) -> Self {
        Self {
            trace_events,
            contexts: BTreeSet::default(),
            frames: vec![],
            block_stack: vec![],
        }
    }

    /// Build a [CallStack] from pre-built frames — used in DAP client mode.
    #[cfg(feature = "dap")]
    pub fn from_remote_frames(frames: Vec<CallFrame>) -> Self {
        Self {
            trace_events: Rc::new(RefCell::new(BTreeMap::new())),
            contexts: BTreeSet::default(),
            frames,
            block_stack: vec![],
        }
    }

    pub fn stacktrace<'a>(
        &'a self,
        recent: &'a VecDeque<Operation>,
        source_manager: &'a dyn SourceManager,
    ) -> StackTrace<'a> {
        StackTrace::new(self, recent, source_manager)
    }

    pub fn current_frame(&self) -> Option<&CallFrame> {
        self.frames.last()
    }

    pub fn current_frame_mut(&mut self) -> Option<&mut CallFrame> {
        self.frames.last_mut()
    }

    pub fn frames(&self) -> &[CallFrame] {
        self.frames.as_slice()
    }

    /// Updates the call stack from `info`
    ///
    /// Returns the call frame exited this cycle, if any
    pub fn next(&mut self, info: &StepInfo<'_>) -> Option<CallFrame> {
        let procedure = info.asmop.map(|op| self.cache_procedure_name(op.context_name()));

        let event = {
            let mut trace_events = self.trace_events.borrow_mut();
            match trace_events.first_key_value() {
                Some((clk, _)) if *clk <= info.clk => {
                    trace_events.pop_first().map(|(_, event)| event)
                }
                _ => None,
            }
        };
        log::trace!(
            "handling {:?}/{:?} at cycle {}: {:?}",
            info.control,
            info.op,
            info.clk,
            &event
        );
        let is_frame_start = event.is_some_and(|event| event.is_frame_start());
        let popped_frame = self.handle_trace_event(event);
        let is_frame_end = popped_frame.is_some();

        match info.control {
            Some(ControlFlowOp::Span) => {
                if let Some(asmop) = info.asmop {
                    log::debug!("{asmop:#?}");
                    self.block_stack.push(Some(SpanContext {
                        frame_index: self.frames.len().saturating_sub(1),
                        location: asmop.location().cloned(),
                    }));
                } else {
                    self.block_stack.push(None);
                }
            }
            Some(ControlFlowOp::Join | ControlFlowOp::Split) => {
                self.block_stack.push(None);
            }
            Some(ControlFlowOp::End) => {
                self.block_stack.pop();
            }
            Some(ControlFlowOp::Respan) | None => {}
        }

        let Some(op) = info.op else {
            return popped_frame;
        };

        if is_frame_start || is_frame_end {
            return popped_frame;
        }

        // Attempt to supply procedure context from the current span context, if needed +
        // available
        let (procedure, asmop) = match procedure {
            proc @ Some(_) => (proc, info.asmop.map(Cow::Borrowed)),
            None => match self.block_stack.last() {
                Some(Some(span_ctx)) => {
                    let proc =
                        self.frames.get(span_ctx.frame_index).and_then(|f| f.procedure.clone());
                    let asmop_cow = info.asmop.map(Cow::Borrowed).or_else(|| {
                        let context_name = proc.as_deref().unwrap_or("<unknown>").to_string();
                        let raw_asmop = AssemblyOp::new(
                            span_ctx.location.clone(),
                            context_name,
                            1,
                            op.to_string(),
                        );
                        Some(Cow::Owned(raw_asmop))
                    });
                    (proc, asmop_cow)
                }
                _ => (None, info.asmop.map(Cow::Borrowed)),
            },
        };

        // Use the current frame's procedure context, if no other more precise context is
        // available
        let procedure = procedure.or_else(|| self.frames.last().and_then(|f| f.procedure.clone()));

        // Do we have a frame? If not, create one
        if self.frames.is_empty() {
            self.frames.push(CallFrame::new(procedure.clone()));
        }

        let current_frame = self.frames.last_mut().unwrap();

        // Does the current frame have a procedure context/location? Use the one from this op if
        // so
        let procedure_context_updated = current_frame.procedure.is_none() && procedure.is_some();
        if procedure_context_updated {
            current_frame.procedure.clone_from(&procedure);
        }

        // Push op into call frame if this is any op other than `nop` or frame setup
        if !matches!(op, Operation::Noop) {
            let cycle_idx = info.asmop.map(|a| a.num_cycles()).unwrap_or(1);
            current_frame.push(op, cycle_idx, asmop.as_deref());
        }

        // Check if we should also update the caller frame's exec detail
        let num_frames = self.frames.len();
        if procedure_context_updated && num_frames > 1 {
            let caller_frame = &mut self.frames[num_frames - 2];
            if let Some(OpDetail::Exec { callee }) = caller_frame.context.back_mut()
                && callee.is_none()
            {
                *callee = procedure;
            }
        }

        popped_frame
    }

    // Get or cache procedure name/context as `Rc<str>`
    fn cache_procedure_name(&mut self, context_name: &str) -> Rc<str> {
        match self.contexts.get(context_name) {
            Some(name) => Rc::clone(name),
            None => {
                let name = Rc::from(context_name.to_string().into_boxed_str());
                self.contexts.insert(Rc::clone(&name));
                name
            }
        }
    }

    fn handle_trace_event(&mut self, event: Option<TraceEvent>) -> Option<CallFrame> {
        // Do we need to handle any frame events?
        if let Some(event) = event {
            match event {
                TraceEvent::FrameStart => {
                    // Record the fact that we exec'd a new procedure in the op context
                    if let Some(current_frame) = self.frames.last_mut() {
                        current_frame.push_exec(None);
                    }
                    // The trace decorator is emitted in the caller, immediately before the exec.
                    // Leave the new frame unnamed until the first callee op provides its context.
                    self.frames.push(CallFrame::new(None));
                }
                TraceEvent::Unknown(code) => log::debug!("unknown trace event: {code}"),
                TraceEvent::FrameEnd => {
                    return self.frames.pop();
                }
                _ => (),
            }
        }
        None
    }
}

pub struct CallFrame {
    procedure: Option<Rc<str>>,
    context: VecDeque<OpDetail>,
    display_name: std::cell::OnceCell<Rc<str>>,
    finishing: bool,
}
impl CallFrame {
    pub fn new(procedure: Option<Rc<str>>) -> Self {
        Self {
            procedure,
            context: Default::default(),
            display_name: Default::default(),
            finishing: false,
        }
    }

    /// Build a frame from remote (DAP) data — used in DAP client mode.
    ///
    /// The frame stores the procedure name and an optional [ResolvedLocation]
    /// as a pre-resolved `OpDetail::Full` entry so that `last_resolved()` and
    /// `recent()` work correctly for pane rendering.
    #[cfg(feature = "dap")]
    pub fn from_remote(name: Option<String>, resolved: Option<ResolvedLocation>) -> Self {
        let procedure = name.map(|n| Rc::from(n.into_boxed_str()));
        let mut context = VecDeque::new();
        if let Some(loc) = resolved {
            let cell = OnceCell::new();
            cell.set(Some(loc)).ok();
            context.push_back(OpDetail::Full {
                op: miden_processor::operation::Operation::Noop,
                location: None,
                resolved: cell,
            });
        }
        Self {
            procedure,
            context,
            display_name: Default::default(),
            finishing: false,
        }
    }

    pub fn procedure(&self, strip_prefix: &str) -> Option<Rc<str>> {
        self.procedure.as_ref()?;
        let name = self.display_name.get_or_init(|| {
            let name = self.procedure.as_deref().unwrap();
            let name = match name.split_once("::") {
                Some((module, rest)) if module == strip_prefix => demangle(rest),
                _ => demangle(name),
            };
            Rc::from(name.into_boxed_str())
        });
        Some(Rc::clone(name))
    }

    pub fn push_exec(&mut self, callee: Option<Rc<str>>) {
        if self.context.len() == 5 {
            self.context.pop_front();
        }

        self.context.push_back(OpDetail::Exec { callee });
    }

    pub fn push(&mut self, opcode: Operation, cycle_idx: u8, op: Option<&AssemblyOp>) {
        if cycle_idx > 1 {
            // Should we ignore this op?
            let skip = self.context.back().map(|detail| matches!(detail, OpDetail::Full { op, .. } | OpDetail::Basic { op } if op == &opcode)).unwrap_or(false);
            if skip {
                return;
            }
        }

        if self.context.len() == 5 {
            self.context.pop_front();
        }

        match op {
            Some(op) => {
                let location = op.location().cloned();
                self.context.push_back(OpDetail::Full {
                    op: opcode,
                    location,
                    resolved: Default::default(),
                });
            }
            None => {
                // If this instruction does not have a location, inherit the location
                // of the previous op in the frame, if one is present
                if let Some(loc) = self.context.back().map(|op| op.location().cloned()) {
                    self.context.push_back(OpDetail::Full {
                        op: opcode,
                        location: loc,
                        resolved: Default::default(),
                    });
                } else {
                    self.context.push_back(OpDetail::Basic { op: opcode });
                }
            }
        }
    }

    pub fn last_location(&self) -> Option<&Location> {
        match self.context.back() {
            Some(OpDetail::Full { location, .. }) => {
                let loc = location.as_ref();
                if loc.is_none() {
                    dbg!(&self.context);
                }
                loc
            }
            Some(OpDetail::Basic { .. }) => None,
            Some(OpDetail::Exec { .. }) => {
                let op = self.context.iter().rev().nth(1)?;
                op.location()
            }
            None => None,
        }
    }

    pub fn last_resolved(&self, source_manager: &dyn SourceManager) -> Option<&ResolvedLocation> {
        // Search through context in reverse order to find the most recent op with a resolvable
        // location.
        for op in self.context.iter().rev() {
            if let Some(resolved) = op.resolve(source_manager) {
                return Some(resolved);
            }
        }
        None
    }

    pub fn recent(&self) -> &VecDeque<OpDetail> {
        &self.context
    }

    #[inline(always)]
    pub fn should_break_on_exit(&self) -> bool {
        self.finishing
    }

    #[inline(always)]
    pub fn break_on_exit(&mut self) {
        self.finishing = true;
    }
}

#[derive(Debug, Clone)]
pub enum OpDetail {
    Full {
        op: Operation,
        location: Option<Location>,
        resolved: OnceCell<Option<ResolvedLocation>>,
    },
    Exec {
        callee: Option<Rc<str>>,
    },
    Basic {
        op: Operation,
    },
}
impl OpDetail {
    pub fn callee(&self, strip_prefix: &str) -> Option<Box<str>> {
        match self {
            Self::Exec { callee: None } => Some(Box::from("<unknown>")),
            Self::Exec {
                callee: Some(callee),
            } => {
                let name = match callee.split_once("::") {
                    Some((module, rest)) if module == strip_prefix => demangle(rest),
                    _ => demangle(callee),
                };
                Some(name.into_boxed_str())
            }
            _ => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Full { op, .. } | Self::Basic { op } => format!("{op}"),
            Self::Exec {
                callee: Some(callee),
            } => format!("exec.{callee}"),
            Self::Exec { callee: None } => "exec.<unavailable>".to_string(),
        }
    }

    pub fn opcode(&self) -> Operation {
        match self {
            Self::Full { op, .. } | Self::Basic { op } => *op,
            Self::Exec { .. } => panic!("no opcode associated with execs"),
        }
    }

    pub fn location(&self) -> Option<&Location> {
        match self {
            Self::Full { location, .. } => location.as_ref(),
            Self::Basic { .. } | Self::Exec { .. } => None,
        }
    }

    pub fn resolve(&self, source_manager: &dyn SourceManager) -> Option<&ResolvedLocation> {
        match self {
            Self::Full {
                location: Some(loc),
                resolved,
                ..
            } => resolved
                .get_or_init(|| {
                    let source_file = resolve_source_file_for_location(source_manager, loc)?;
                    let span = SourceSpan::new(source_file.id(), loc.start..loc.end);
                    let file_line_col = source_file.location(span);
                    Some(ResolvedLocation {
                        source_file,
                        line: file_line_col.line.to_u32(),
                        col: file_line_col.column.to_u32(),
                        span,
                    })
                })
                .as_ref(),
            _ => None,
        }
    }
}

/// Resolve a source file for `location`.
///
/// Compiled packages may contain remapped paths such as `src/lib.rs`, while sources loaded by the
/// VM host may be keyed by an absolute path, or may not be loaded yet at all. Prefer the source
/// manager's existing URI table, then fall back to loading the file from disk.
pub fn resolve_source_file_for_location(
    source_manager: &dyn SourceManager,
    location: &Location,
) -> Option<Arc<SourceFile>> {
    source_manager.get_by_uri(location.uri()).or_else(|| {
        resolve_source_path(location.uri()).and_then(|path| source_manager.load_file(&path).ok())
    })
}

/// Resolve a source URI to an existing local filesystem path.
///
/// Non-file URI schemes are left to the source manager. Relative paths are resolved against the
/// debugger process' current directory, which DAP clients set to the launch `cwd`.
pub fn resolve_source_path(uri: &Uri) -> Option<PathBuf> {
    let path = match uri.scheme() {
        None | Some("file") => Path::new(uri.path()),
        Some(_) => return None,
    };

    existing_path(path).or_else(|| {
        if path.is_relative() {
            std::env::current_dir().ok().and_then(|cwd| existing_path(&cwd.join(path)))
        } else {
            None
        }
    })
}

/// Resolve a source location directly from the filesystem, returning the resolved path and line.
pub fn resolve_location_from_filesystem(location: &Location) -> Option<(PathBuf, u32)> {
    let path = resolve_source_path(location.uri())?;
    let bytes = std::fs::read(&path).ok()?;
    let start = location.start.to_usize().min(bytes.len());
    let line = bytes[..start].iter().filter(|byte| **byte == b'\n').count() as u32 + 1;
    Some((path, line))
}

/// Returns true for source paths emitted by compiler/runtime internals rather than user code.
pub fn is_internal_source_uri(uri: &Uri) -> bool {
    let path = uri.path().replace('\\', "/");
    path.contains("/codegen/masm/intrinsics/") || path.contains("/rustlib/src/rust/library/")
}

fn existing_path(path: &Path) -> Option<PathBuf> {
    path.exists()
        .then(|| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

#[derive(Debug, Clone)]
pub struct ResolvedLocation {
    pub source_file: Arc<SourceFile>,
    // TODO(fabrio): Use LineNumber and ColumnNumber instead of raw `u32`.
    pub line: u32,
    pub col: u32,
    pub span: SourceSpan,
}
impl fmt::Display for ResolvedLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.source_file.uri().as_str(), self.line, self.col)
    }
}

pub struct CurrentFrame {
    pub procedure: Option<Rc<str>>,
    pub location: Option<ResolvedLocation>,
}

pub struct StackTrace<'a> {
    callstack: &'a CallStack,
    recent: &'a VecDeque<Operation>,
    source_manager: &'a dyn SourceManager,
    current_frame: Option<CurrentFrame>,
}

impl<'a> StackTrace<'a> {
    pub fn new(
        callstack: &'a CallStack,
        recent: &'a VecDeque<Operation>,
        source_manager: &'a dyn SourceManager,
    ) -> Self {
        let current_frame = callstack.current_frame().map(|frame| {
            let location = frame.last_resolved(source_manager).cloned();
            let procedure = frame.procedure("");
            CurrentFrame {
                procedure,
                location,
            }
        });
        Self {
            callstack,
            recent,
            source_manager,
            current_frame,
        }
    }

    pub fn current_frame(&self) -> Option<&CurrentFrame> {
        self.current_frame.as_ref()
    }
}

impl fmt::Display for StackTrace<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use std::fmt::Write;

        let num_frames = self.callstack.frames.len();

        writeln!(f, "\nStack Trace:")?;

        for (i, frame) in self.callstack.frames.iter().enumerate() {
            let is_top = i + 1 == num_frames;
            let name = frame.procedure("");
            let name = name.as_deref().unwrap_or("<unknown>");
            if is_top {
                write!(f, " `-> {name}")?;
            } else {
                write!(f, " |-> {name}")?;
            }
            if let Some(resolved) = frame.last_resolved(self.source_manager) {
                write!(f, " in {resolved}")?;
            } else {
                write!(f, " in <unavailable>")?;
            }
            if is_top {
                // Print op context
                let context_size = frame.context.len();
                writeln!(f, ":\n\nLast {context_size} Instructions (of current frame):")?;
                for (i, op) in frame.context.iter().enumerate() {
                    let is_last = i + 1 == context_size;
                    if let Some(callee) = op.callee("") {
                        write!(f, " |   exec.{callee}")?;
                    } else {
                        write!(f, " |   {}", &op.opcode())?;
                    }
                    if is_last {
                        writeln!(f, "\n `-> <error occured here>")?;
                    } else {
                        f.write_char('\n')?;
                    }
                }

                let context_size = self.recent.len();
                writeln!(f, "\n\nLast {context_size} Instructions (any frame):")?;
                for (i, op) in self.recent.iter().enumerate() {
                    let is_last = i + 1 == context_size;
                    if is_last {
                        writeln!(f, " |   {}", &op)?;
                        writeln!(f, " `-> <error occured here>")?;
                    } else {
                        writeln!(f, " |   {}", &op)?;
                    }
                }
            } else {
                f.write_char('\n')?;
            }
        }

        Ok(())
    }
}

fn demangle(name: &str) -> String {
    let mut input = name.as_bytes();
    let mut demangled = Vec::with_capacity(input.len() * 2);
    rustc_demangle::demangle_stream(&mut input, &mut demangled, /* include_hash= */ false)
        .expect("failed to write demangled identifier");
    String::from_utf8(demangled).expect("demangled identifier contains invalid utf-8")
}

#[cfg(test)]
mod tests {
    use std::{cell::OnceCell, fs, path::PathBuf};

    use miden_assembly::DefaultSourceManager;
    use miden_debug_types::{ByteIndex, Location, Uri};

    use super::*;

    #[test]
    fn resolves_relative_source_locations_from_filesystem() {
        let path = test_source_path("relative");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fn main() {\n    let x = 1;\n}\n").unwrap();

        let start = "fn main() {\n    ".len() as u32;
        let location = Location::new(
            Uri::from(path.display().to_string()),
            ByteIndex::new(start),
            ByteIndex::new(start + 5),
        );
        let detail = OpDetail::Full {
            op: Operation::Noop,
            location: Some(location),
            resolved: OnceCell::new(),
        };
        let source_manager = DefaultSourceManager::default();

        let resolved = detail.resolve(&source_manager).expect("source should resolve");
        assert_eq!(resolved.line, 2);
        assert!(resolved.source_file.uri().as_str().ends_with("src/lib.rs"));

        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).ok();
    }

    fn test_source_path(test_name: &str) -> PathBuf {
        PathBuf::from("target")
            .join("debugger-source-tests")
            .join(format!("{}-{}", test_name, std::process::id()))
            .join("src")
            .join("lib.rs")
    }
}
