use alloc::{string::String, sync::Arc, vec::Vec};

/// The hierarchy of calls made during an execution.
#[derive(Debug, Default)]
pub struct CallTrace {
    pub roots: Vec<CallFrameRecord>,
}

/// A single call, with the calls it made nested underneath.
#[derive(Debug)]
pub struct CallFrameRecord {
    pub callee: Option<String>,
    pub enter_clk: usize,
    pub exit_clk: Option<usize>,
    pub args: Vec<TracedArg>,
    /// Returned values, top of the stack first, or `None` when they could not be worked out.
    pub results: Option<Vec<u64>>,
    pub children: Vec<CallFrameRecord>,
}

/// A single argument of a traced call.
#[derive(Debug)]
pub struct TracedArg {
    pub index: u32,
    pub name: String,
    /// How many stack elements the argument takes, or `None` if its type is not known.
    pub felt_count: Option<usize>,
    /// The argument's felts, or `None` if its width is not known or they could not be read.
    pub values: Option<Vec<u64>>,
}

impl CallFrameRecord {
    /// Number of cycles spent in this frame, including its children.
    pub fn cycles(&self) -> Option<usize> {
        self.exit_clk.map(|exit| exit.saturating_sub(self.enter_clk))
    }
}

/// Builds a [`CallTrace`] from frame entries and exits reported in execution order.
#[derive(Default)]
pub struct CallTraceRecorder {
    open: Vec<OpenFrame>,
    roots: Vec<CallFrameRecord>,
}

struct OpenFrame {
    record: CallFrameRecord,
    caller: Option<Arc<str>>,
    enter_depth: usize,
    /// Set once this frame makes a call, after which arguments are no longer recorded.
    args_sealed: bool,
}

impl CallTraceRecorder {
    /// Open a frame entered at `clk`.
    pub fn enter(&mut self, clk: usize, caller: Option<Arc<str>>, stack_depth: usize) {
        if let Some(parent) = self.open.last_mut() {
            parent.args_sealed = true;
        }
        self.open.push(OpenFrame {
            record: CallFrameRecord {
                callee: None,
                enter_clk: clk,
                exit_clk: None,
                args: Vec::new(),
                results: None,
                children: Vec::new(),
            },
            caller,
            enter_depth: stack_depth,
            args_sealed: false,
        });
    }

    /// Name the innermost call after the first name that is not the caller, and keep that name.
    ///
    /// The markers sit in the caller, around the call, so the caller runs first and last:
    ///
    /// ```text
    /// fn function_1() -> Felt { function_2() }
    ///
    /// function_1   the start marker runs      -> same as the caller, skipped
    /// function_2   the callee runs            -> this names the call
    /// function_1   the callee has returned    -> the call has a name, skipped
    /// ```
    ///
    /// Without the caller check the call is named after its caller; without the `is_none` check
    /// it is renamed back to the caller once the callee returns.
    pub fn observe_name(&mut self, name: &str) {
        if let Some(frame) = self.open.last_mut()
            && frame.record.callee.is_none()
            && frame.caller.as_deref() != Some(name)
        {
            frame.record.callee = Some(String::from(name));
        }
    }

    /// The cycle the innermost open frame was entered at, while it still takes arguments.
    ///
    /// `None` once that frame has made a call of its own, which seals its arguments.
    pub fn accepting_args(&self) -> Option<usize> {
        self.open
            .last()
            .filter(|frame| !frame.args_sealed)
            .map(|frame| frame.record.enter_clk)
    }

    /// Whether the innermost open frame already holds the argument at `index`.
    ///
    /// Lets the caller skip re-reading the argument from VM memory on every cycle, since
    /// [`Self::observe_arg`] keeps the first value observed.
    pub fn has_arg(&self, index: u32) -> bool {
        self.open
            .last()
            .is_some_and(|frame| frame.record.args.iter().any(|arg| arg.index == index))
    }

    /// Record an argument of the innermost open frame, keeping the value the call was made with.
    ///
    /// `felt_count` is how many stack elements the argument takes - [`Self::exit`] counts it
    /// back to work out the results - or `None` when the width is not known.
    ///
    /// A parameter is a local variable, so the body can overwrite it; only the first observation
    /// is kept.
    ///
    /// ```text
    /// fn function_1(mut op: Felt, amount: Felt) -> Felt { op = 99; op + amount }
    ///
    /// op = 3    the call was made with this   -> kept
    /// op = 99   the body wrote this later     -> skipped
    /// ```
    pub fn observe_arg(
        &mut self,
        index: u32,
        name: &str,
        felt_count: Option<usize>,
        values: Option<Vec<u64>>,
    ) {
        let Some(frame) = self.open.last_mut() else {
            return;
        };
        if frame.args_sealed || frame.record.args.iter().any(|arg| arg.index == index) {
            return;
        }
        frame.record.args.push(TracedArg {
            index,
            name: String::from(name),
            felt_count,
            values,
        });
    }

    /// Close the innermost open frame at `clk`, reading its results from `stack`.
    ///
    /// Arguments are counted by width, not one element each: `Felt` 1, `u64` 2, `Word` 4. One
    /// argument of unknown width makes the total unknown, and the results with it.
    ///
    /// ```text
    /// fn function_1(op: Felt, amount: Felt) -> Felt { op + amount }   // function_1(3, 5)
    ///
    /// stack 18 -> 17, two arguments   -> one result, 8
    /// stack 17 -> 16, one argument    -> no results
    /// stack 17 -> 16, no arguments    -> not known, the call popped more than it pushed
    ///
    /// fn function_2(x: u64) -> u64 { .. }
    ///
    /// stack 18 -> 18, one argument two elements wide   -> two results
    /// stack 18 -> 18, one argument of unknown width    -> not known
    /// ```
    pub fn exit(&mut self, clk: usize, stack: &[u64]) {
        let Some(frame) = self.open.pop() else {
            return;
        };
        let mut record = frame.record;
        record.exit_clk = Some(clk);

        // A call pops its arguments and pushes its results, so `popped + args_width` is how many
        // elements it left. A negative count means not all arguments were recorded, and a `None`
        // width means one of them has no known size; either way the results are unknown.
        let popped = stack.len() as isize - frame.enter_depth as isize;
        let args_width: Option<usize> = record.args.iter().map(|arg| arg.felt_count).sum();
        record.results = args_width
            .and_then(|width| usize::try_from(popped + width as isize).ok())
            .map(|count| stack.iter().take(count).copied().collect());

        self.attach(record);
    }

    /// Return the trace, leaving frames that are still open without an exit cycle.
    pub fn finish(mut self) -> CallTrace {
        while let Some(frame) = self.open.pop() {
            self.attach(frame.record);
        }
        CallTrace { roots: self.roots }
    }

    /// Put a closed frame in its place in the tree, with its arguments in signature order.
    fn attach(&mut self, mut frame: CallFrameRecord) {
        frame.args.sort_by_key(|arg| arg.index);
        match self.open.last_mut() {
            Some(parent) => parent.record.children.push(frame),
            None => self.roots.push(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::CallTraceRecorder;

    fn stack(depth: usize, top: &[u64]) -> Vec<u64> {
        let mut stack = top.to_vec();
        stack.resize(depth, 0);
        stack
    }

    /// An argument one stack element wide, as a `Felt` parameter is.
    fn observe_felt_arg(recorder: &mut CallTraceRecorder, index: u32, name: &str, value: u64) {
        recorder.observe_arg(index, name, Some(1), Some(vec![value]));
    }

    /// ```text
    /// fn function_1() -> (Felt, Felt) { (function_2(), function_3()) }
    ///
    /// function_2 -> 42
    /// function_3 -> 7
    /// ```
    ///
    /// Both go under `function_1`, in the order they were called.
    #[test]
    fn frames_follow_entry_and_exit_order() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 16);
        recorder.observe_name("$main");
        recorder.observe_name("function_1");
        recorder.enter(20, Some("function_1".into()), 16);
        recorder.observe_name("function_2");
        recorder.exit(30, &stack(17, &[42]));
        recorder.enter(40, Some("function_1".into()), 17);
        recorder.observe_name("function_3");
        recorder.exit(50, &stack(18, &[7, 42]));
        recorder.exit(60, &stack(18, &[7, 42]));

        let trace = recorder.finish();

        assert_eq!(trace.roots.len(), 1);
        let function_1 = &trace.roots[0];
        assert_eq!(function_1.callee.as_deref(), Some("function_1"));
        assert_eq!((function_1.enter_clk, function_1.exit_clk), (10, Some(60)));
        assert_eq!(function_1.results.as_deref(), Some([7, 42].as_slice()));

        let names: Vec<_> =
            function_1.children.iter().map(|frame| frame.callee.as_deref()).collect();
        assert_eq!(names, [Some("function_2"), Some("function_3")]);
        assert_eq!(function_1.children[0].results.as_deref(), Some([42].as_slice()));
        assert_eq!(function_1.children[1].results.as_deref(), Some([7].as_slice()));
    }

    /// ```text
    /// fn function_2(x: Felt) -> Felt { load_sw(x) }   // load_sw is MASM, it has no markers
    ///
    /// function_2                 the callee runs   -> names the call
    /// intrinsics::mem::load_sw   it calls this     -> skipped
    /// ```
    ///
    /// `load_sw` gets no call of its own, and must not rename this one.
    #[test]
    fn an_unmarked_call_does_not_rename_the_frame_it_runs_in() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("function_1".into()), 16);
        recorder.observe_name("function_1");
        recorder.observe_name("function_2");
        recorder.observe_name("intrinsics::mem::load_sw");
        recorder.exit(20, &stack(17, &[42]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].callee.as_deref(), Some("function_2"));
    }

    /// ```text
    /// fn function_1() { function_2(); panic!() }
    ///
    /// function_2   returns   -> has an exit cycle
    /// function_1   fails     -> has none
    /// ```
    ///
    /// The trace still shows the call that failed.
    #[test]
    fn a_frame_left_open_has_no_exit() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 16);
        recorder.observe_name("function_1");
        recorder.enter(20, Some("function_1".into()), 16);
        recorder.observe_name("function_2");
        recorder.exit(30, &stack(17, &[42]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.exit_clk, None);
        assert_eq!(function_1.cycles(), None);
        assert_eq!(function_1.children[0].exit_clk, Some(30));
    }

    /// ```text
    /// fn function_1(a: Felt) { function_2(7, 9) }
    /// fn function_2(b: Felt, c: Felt) { .. }
    ///
    /// a = 5   function_1's own parameter          -> kept
    /// c = 9   still reported after function_2 ran -> skipped
    /// ```
    ///
    /// `function_1` keeps only `a` - see `args_sealed`.
    #[test]
    fn arguments_are_not_collected_after_the_frame_makes_a_call() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 17);
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 1, "a", 5);
        recorder.enter(20, Some("function_1".into()), 18);
        recorder.observe_name("function_2");
        observe_felt_arg(&mut recorder, 1, "b", 7);
        observe_felt_arg(&mut recorder, 2, "c", 9);
        recorder.exit(30, &stack(17, &[0]));
        observe_felt_arg(&mut recorder, 2, "c", 9);
        recorder.exit(40, &stack(17, &[0]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        let names: Vec<_> = function_1.args.iter().map(|arg| arg.name.as_str()).collect();
        assert_eq!(names, ["a"]);
        let function_2 = &function_1.children[0];
        let names: Vec<_> = function_2.args.iter().map(|arg| arg.name.as_str()).collect();
        assert_eq!(names, ["b", "c"]);
    }

    /// `has_arg` answers for the innermost open frame only, so the caller can skip a read it
    /// would throw away.
    #[test]
    fn an_argument_already_recorded_is_reported_as_held() {
        let mut recorder = CallTraceRecorder::default();
        assert!(!recorder.has_arg(1), "no frame is open");

        recorder.enter(10, Some("$main".into()), 17);
        assert!(!recorder.has_arg(1));
        observe_felt_arg(&mut recorder, 1, "a", 3);
        assert!(recorder.has_arg(1));
        assert!(!recorder.has_arg(2));

        // The inner frame holds none of the outer frame's arguments.
        recorder.enter(20, Some("function_1".into()), 17);
        assert!(!recorder.has_arg(1));
    }

    /// ```text
    /// fn function_1(mut op: Felt, amount: Felt) -> Felt { op = 99; .. }   // function_1(3, 5)
    ///
    /// amount = 5   comes first, but is the second parameter
    /// op = 3       the call was made with this   -> kept
    /// op = 99      the body wrote this later     -> skipped
    /// ```
    ///
    /// The trace shows `op = 3, amount = 5`.
    #[test]
    fn arguments_are_ordered_and_the_first_observation_is_kept() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 18);
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 2, "amount", 5);
        observe_felt_arg(&mut recorder, 1, "op", 3);
        observe_felt_arg(&mut recorder, 1, "op", 99);
        recorder.exit(20, &stack(17, &[8]));

        let trace = recorder.finish();

        let args: Vec<_> = trace.roots[0]
            .args
            .iter()
            .map(|arg| (arg.index, arg.name.as_str(), arg.values.as_deref()))
            .collect();
        assert_eq!(args, [(1, "op", Some([3].as_slice())), (2, "amount", Some([5].as_slice()))]);
    }

    /// ```text
    /// fn function_1(op: Felt, amount: Felt) -> Felt { op + amount }   // function_1(3, 5)
    ///
    /// stack 18 -> 17   two arguments off, 8 on
    /// ```
    ///
    /// One result is reported.
    #[test]
    fn results_are_derived_from_the_stack_depth_and_the_arguments() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 18);
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 1, "a", 3);
        observe_felt_arg(&mut recorder, 2, "b", 5);
        recorder.exit(20, &stack(17, &[8]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([8].as_slice()));
    }

    /// An argument the debug information gives no type for has no width, and a call holding one
    /// reports no results: the stack alone does not say how much of it was the argument.
    #[test]
    fn an_argument_of_unknown_width_leaves_the_results_unknown() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 18);
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 1, "a", 3);
        recorder.observe_arg(2, "b", None, None);
        recorder.exit(20, &stack(17, &[8]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.results, None);
        assert_eq!(function_1.args[1].felt_count, None);
        assert_eq!(function_1.args[1].values, None);
    }

    /// ```text
    /// fn function_1(x: u64) -> u64 { .. }   // function_1(1 << 32)
    ///
    /// stack 18 -> 18   one argument, two elements off and two on
    /// ```
    ///
    /// The argument is one entry but two stack elements, so two results are reported, not one.
    #[test]
    fn a_wide_argument_counts_for_the_stack_it_takes() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 18);
        recorder.observe_name("function_1");
        recorder.observe_arg(1, "x", Some(2), Some(vec![0, 1]));
        recorder.exit(20, &stack(18, &[9, 8]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.results.as_deref(), Some([9, 8].as_slice()));
        assert_eq!(function_1.args[0].values.as_deref(), Some([0, 1].as_slice()));
    }

    /// ```text
    /// fn function_1(w: Word, n: Felt) { .. }
    ///
    /// stack 21 -> 16   five elements off, nothing on
    /// ```
    ///
    /// Counting entries instead of widths would put this below zero and report the results as
    /// unknown; counting widths gives the right answer: none.
    #[test]
    fn a_call_taking_wide_arguments_and_returning_nothing_reports_no_results() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 21);
        recorder.observe_name("function_1");
        recorder.observe_arg(1, "w", Some(4), Some(vec![1, 2, 3, 4]));
        observe_felt_arg(&mut recorder, 2, "n", 5);
        recorder.exit(20, &stack(16, &[99]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([].as_slice()));
    }

    /// ```text
    /// fn function_1(a: Felt) { .. }   // function_1(3)
    ///
    /// stack 17 -> 16   the argument off, nothing on
    /// ```
    ///
    /// The 99 left on top belongs to the caller, so no result is reported.
    #[test]
    fn a_call_without_results_reports_none() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 17);
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 1, "a", 3);
        recorder.exit(20, &stack(16, &[99]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([].as_slice()));
    }

    /// ```text
    /// fn function_1(a: Felt) -> Felt { .. }   // from a package with no debug information
    ///
    /// stack 17 -> 16   the argument off and one result on, but no arguments are reported
    /// ```
    ///
    /// The count comes out below zero, so the results are not known.
    #[test]
    fn results_are_unknown_when_the_arguments_were_not_recorded() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()), 17);
        recorder.observe_name("function_1");
        recorder.exit(20, &stack(16, &[8]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results, None);
    }
}
