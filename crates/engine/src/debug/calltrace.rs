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
    /// Frames that started and did not end yet. The last one is the current frame.
    open: Vec<OpenFrame>,
    /// Frames that ended when no parent frame was open.
    roots: Vec<CallFrameRecord>,
}

struct OpenFrame {
    record: CallFrameRecord,
    caller: Option<Arc<str>>,
    /// Felts the callee returns; `None` means unknown.
    output_felt_count: Option<usize>,
    /// A `call`/`dynexec` entered this frame, so the callee may share the caller's name (recursion).
    boundary_crossed: bool,
    /// Set once this frame makes a call, after which arguments are no longer recorded.
    args_sealed: bool,
}

impl CallTraceRecorder {
    /// Open a frame entered at `clk`.
    pub fn enter(&mut self, clk: usize, caller: Option<Arc<str>>) {
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
            output_felt_count: None,
            boundary_crossed: false,
            args_sealed: false,
        });
    }

    /// Names the innermost call after the first name that is not the caller's (or any name after
    /// [`Self::mark_call_boundary`], for recursion). Returns `true` when it sets the name.
    ///
    /// ```text
    /// fn function_1() -> Felt { function_2() }
    ///
    /// function_1   start marker   -> caller, skipped
    /// function_2   callee runs    -> names the call
    /// function_1   after return   -> already named, skipped
    ///
    /// fn fib(n) -> Felt { fib(n - 1) + .. }   // dynexec to itself
    ///
    /// fib          start marker   -> caller, skipped
    /// fib          after boundary -> names the call
    /// ```
    ///
    /// The name is wrong if the callee has no debug info or starts with `exec`.
    pub fn observe_name(&mut self, name: &str) -> bool {
        if let Some(frame) = self.open.last_mut()
            && frame.record.callee.is_none()
            && (frame.boundary_crossed || frame.caller.as_deref() != Some(name))
        {
            frame.record.callee = Some(String::from(name));
            return true;
        }
        false
    }

    /// A `call`/`dynexec` entered the innermost frame, so its callee may share the caller's name.
    pub fn mark_call_boundary(&mut self) {
        if let Some(frame) = self.open.last_mut() {
            frame.boundary_crossed = true;
        }
    }

    /// Sets how many felts the innermost callee returns; `None` keeps results unknown.
    pub fn observe_output_width(&mut self, count: Option<usize>) {
        if let Some(frame) = self.open.last_mut()
            && frame.output_felt_count.is_none()
        {
            frame.output_felt_count = count;
        }
    }

    /// The trace's first frame, whose result width comes from the manifest.
    pub fn is_first_frame(&self) -> bool {
        self.open.len() == 1 && self.roots.is_empty()
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

    /// Records an argument of the innermost frame. Only the first value is kept, because the body
    /// can overwrite a parameter.
    ///
    /// ```text
    /// fn function_1(mut op: Felt) { op = 99; .. }
    ///
    /// op = 3    value at the call   -> kept
    /// op = 99   written by the body -> skipped
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

    /// Closes the innermost frame at `clk`; its results are the top
    /// [`Self::observe_output_width`] felts of `stack`.
    ///
    /// ```text
    /// -> Felt          width 1     -> results = [top felt]
    /// returns nothing  width 0     -> results = []
    /// no signature     width None  -> results = None (unknown)
    /// ```
    pub fn exit(&mut self, clk: usize, stack: &[u64]) {
        let Some(frame) = self.open.pop() else {
            return;
        };
        let mut record = frame.record;
        record.exit_clk = Some(clk);

        // If the stack is shorter than the width, the width is wrong, so the result is unknown.
        record.results = frame
            .output_felt_count
            .and_then(|count| (stack.len() >= count).then(|| stack[..count].to_vec()));

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
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("$main");
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(2)); // function_1 returns (Felt, Felt)
        recorder.enter(20, Some("function_1".into()));
        recorder.observe_name("function_2");
        recorder.observe_output_width(Some(1)); // function_2 returns Felt
        recorder.exit(30, &stack(17, &[42]));
        recorder.enter(40, Some("function_1".into()));
        recorder.observe_name("function_3");
        recorder.observe_output_width(Some(1)); // function_3 returns Felt
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
    /// fn consume_u64(n: u64)   // stack 16 -> 16
    /// ```
    ///
    /// Padding at the minimum stack depth is not reported as results.
    #[test]
    fn min_depth_padding_is_not_counted_as_result() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("caller".into()));
        recorder.observe_name("consume_u64");
        recorder.observe_arg(0, "n", Some(2), Some(vec![0, 0]));
        recorder.observe_output_width(Some(0));
        recorder.exit(20, &stack(16, &[]));

        let consume = &recorder.finish().roots[0];
        assert_eq!(consume.results.as_deref(), Some([].as_slice()));
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
        recorder.enter(10, Some("function_1".into()));
        recorder.observe_name("function_1");
        recorder.observe_name("function_2");
        recorder.observe_name("intrinsics::mem::load_sw");
        recorder.exit(20, &stack(17, &[42]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].callee.as_deref(), Some("function_2"));
    }

    /// ```text
    /// fn fib(n: Felt) -> Felt { fib(n - 1) + fib(n - 2) }   // dynexec to itself
    /// ```
    ///
    /// After a call boundary, a callee with the caller's name is still named.
    #[test]
    fn a_recursive_call_is_named_after_the_call_boundary() {
        fn fib_value(n: u64) -> u64 {
            if n < 2 {
                n
            } else {
                fib_value(n - 1) + fib_value(n - 2)
            }
        }

        // Same call order as `collect_trace` for a `dynexec` self-call.
        fn drive(recorder: &mut CallTraceRecorder, n: u64, clk: &mut usize) {
            *clk += 10;
            recorder.enter(*clk, Some("fib".into()));
            recorder.mark_call_boundary();
            recorder.observe_name("fib");
            recorder.observe_output_width(Some(1));
            if n >= 2 {
                drive(recorder, n - 1, clk);
                drive(recorder, n - 2, clk);
            }
            *clk += 10;
            recorder.exit(*clk, &stack(16, &[fib_value(n)]));
        }

        let mut recorder = CallTraceRecorder::default();
        drive(&mut recorder, 4, &mut 0);
        let root = recorder.finish().roots.remove(0);

        assert_eq!(root.callee.as_deref(), Some("fib"));
        assert_eq!(root.results.as_deref(), Some([3].as_slice())); // fib(4) = 3
        assert_eq!(root.children.len(), 2);

        let fib3 = &root.children[0];
        let fib2 = &root.children[1];
        assert_eq!(fib3.callee.as_deref(), Some("fib"));
        assert_eq!(fib3.results.as_deref(), Some([2].as_slice())); // fib(3) = 2
        assert_eq!(fib2.callee.as_deref(), Some("fib"));
        assert_eq!(fib2.results.as_deref(), Some([1].as_slice())); // fib(2) = 1

        let deep = &fib3.children[0];
        assert_eq!(deep.callee.as_deref(), Some("fib")); // fib(2)
        assert_eq!(deep.children[0].results.as_deref(), Some([1].as_slice())); // fib(1) = 1
        assert_eq!(deep.children[1].results.as_deref(), Some([0].as_slice())); // fib(0) = 0
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
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.enter(20, Some("function_1".into()));
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
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        observe_felt_arg(&mut recorder, 1, "a", 5);
        recorder.enter(20, Some("function_1".into()));
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

        recorder.enter(10, Some("$main".into()));
        assert!(!recorder.has_arg(1));
        observe_felt_arg(&mut recorder, 1, "a", 3);
        assert!(recorder.has_arg(1));
        assert!(!recorder.has_arg(2));

        // The inner frame holds none of the outer frame's arguments.
        recorder.enter(20, Some("function_1".into()));
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
        recorder.enter(10, Some("$main".into()));
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
    /// fn function_1(op: Felt, amount: Felt) -> Felt { op + amount }   // returns one felt
    /// ```
    ///
    /// The signature says one result, so the top felt of the stack is reported.
    #[test]
    fn a_felt_result_is_the_top_of_the_stack() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(1));
        observe_felt_arg(&mut recorder, 1, "a", 3);
        observe_felt_arg(&mut recorder, 2, "b", 5);
        recorder.exit(20, &stack(17, &[8]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([8].as_slice()));
    }

    /// Argument width no longer drives the results: an argument of unknown width still leaves the
    /// results known, because the count comes from the callee's signature.
    #[test]
    fn an_argument_of_unknown_width_does_not_affect_the_results() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(1));
        observe_felt_arg(&mut recorder, 1, "a", 3);
        recorder.observe_arg(2, "b", None, None);
        recorder.exit(20, &stack(17, &[8]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.results.as_deref(), Some([8].as_slice()));
        assert_eq!(function_1.args[1].felt_count, None);
        assert_eq!(function_1.args[1].values, None);
    }

    /// ```text
    /// fn function_1(x: u64) -> u64 { .. }   // returns a u64
    /// ```
    ///
    /// A `u64` result is two felts, so the top two of the stack are reported.
    #[test]
    fn a_u64_result_is_two_felts() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(2));
        recorder.observe_arg(1, "x", Some(2), Some(vec![0, 1]));
        recorder.exit(20, &stack(18, &[9, 8]));

        let trace = recorder.finish();

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.results.as_deref(), Some([9, 8].as_slice()));
        assert_eq!(function_1.args[0].values.as_deref(), Some([0, 1].as_slice()));
    }

    /// ```text
    /// fn function_1(w: Word, n: Felt) { .. }   // returns nothing
    /// ```
    ///
    /// The signature says no results, so none are reported and the 99 left on top of the stack is
    /// ignored.
    #[test]
    fn a_call_that_returns_nothing_reports_no_results() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(0));
        recorder.observe_arg(1, "w", Some(4), Some(vec![1, 2, 3, 4]));
        observe_felt_arg(&mut recorder, 2, "n", 5);
        recorder.exit(20, &stack(16, &[99]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([].as_slice()));
    }

    /// ```text
    /// fn function_1(a: Felt) { .. }   // returns nothing
    /// ```
    ///
    /// The 99 left on top belongs to the caller, and the signature says no results, so none are
    /// reported.
    #[test]
    fn a_call_without_results_reports_empty() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.observe_output_width(Some(0));
        observe_felt_arg(&mut recorder, 1, "a", 3);
        recorder.exit(20, &stack(16, &[99]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results.as_deref(), Some([].as_slice()));
    }

    /// A call whose callee has no signature has no output width, so its results are unknown rather
    /// than guessed from the stack.
    #[test]
    fn results_are_unknown_without_a_signature() {
        let mut recorder = CallTraceRecorder::default();
        recorder.enter(10, Some("$main".into()));
        recorder.observe_name("function_1");
        recorder.exit(20, &stack(16, &[8]));

        let trace = recorder.finish();

        assert_eq!(trace.roots[0].results, None);
    }
}
