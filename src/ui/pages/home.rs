use std::{boxed::Box, string::ToString, vec::Vec};

use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent},
    prelude::*,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    debug::{BreakpointType, ReadMemoryExpr},
    ui::{
        action::Action,
        pages::Page,
        panes::{
            Pane, breakpoints::BreakpointsPane, disasm::DisassemblyPane,
            source_code::SourceCodePane, stack::OperandStackPane, stacktrace::StackTracePane,
        },
        state::{InputMode, State},
        tui::EventResponse,
    },
};

#[derive(Default)]
pub struct Home {
    command_tx: Option<UnboundedSender<Action>>,
    panes: Vec<Box<dyn Pane>>,
    focused_pane_index: usize,
    fullscreen_pane_index: Option<usize>,
}

impl Home {
    pub fn new() -> Result<Self, Report> {
        let focused_border_style = Style::default().fg(Color::LightGreen);

        Ok(Self {
            command_tx: None,
            panes: vec![
                Box::new(SourceCodePane::new(true, focused_border_style)),
                Box::new(DisassemblyPane::new(false, focused_border_style)),
                Box::new(StackTracePane::new(false, focused_border_style)),
                Box::new(OperandStackPane::new(false, focused_border_style)),
                Box::new(BreakpointsPane::new(false, focused_border_style)),
            ],

            focused_pane_index: 0,
            fullscreen_pane_index: None,
        })
    }
}

impl Page for Home {
    fn init(&mut self, state: &State) -> Result<(), Report> {
        for pane in self.panes.iter_mut() {
            pane.init(state)?;
        }
        Ok(())
    }

    fn focus(&mut self) -> Result<(), Report> {
        if let Some(command_tx) = &self.command_tx {
            const ARROW: &str = symbols::scrollbar::HORIZONTAL.end;
            let status_line =
                format!("[l,h {ARROW} pane movement] [: {ARROW} commands] [q {ARROW} quit]");
            command_tx.send(Action::StatusLine(status_line)).into_diagnostic()?;
        }
        Ok(())
    }

    fn register_action_handler(&mut self, tx: UnboundedSender<Action>) -> Result<(), Report> {
        self.command_tx = Some(tx);
        Ok(())
    }

    fn update(&mut self, action: Action, state: &mut State) -> Result<Option<Action>, Report> {
        let mut actions: Vec<Option<Action>> = vec![];
        match action {
            Action::Tick => {}
            Action::FocusNext => {
                let next_index = self.focused_pane_index.saturating_add(1) % self.panes.len();
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::UnFocus, state)?);
                }
                self.focused_pane_index = next_index;
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::Focus, state)?);
                }
            }
            Action::FocusPrev => {
                let prev_index =
                    self.focused_pane_index.saturating_add(self.panes.len() - 1) % self.panes.len();
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::UnFocus, state)?);
                }
                self.focused_pane_index = prev_index;
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::Focus, state)?);
                }
            }
            Action::Update => {
                for pane in self.panes.iter_mut() {
                    actions.push(pane.update(action.clone(), state)?);
                }
            }
            Action::ToggleFullScreen => {
                self.fullscreen_pane_index =
                    self.fullscreen_pane_index.map_or(Some(self.focused_pane_index), |_| None);
            }
            Action::FocusFooter(..) => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::UnFocus, state)?);
                }
            }
            Action::FooterResult(cmd, Some(args)) if cmd.eq(":") => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    pane.update(Action::Focus, state)?;
                }
                // Dispatch commands of the form: CMD [ARGS..]
                match args.split_once(' ') {
                    Some((cmd, rest)) => match cmd.trim() {
                        "b" | "break" | "breakpoint" => match rest.parse::<BreakpointType>() {
                            Ok(ty) => {
                                state.create_breakpoint(ty);
                                actions.push(Some(Action::TimedStatusLine(
                                    "breakpoint created".to_string(),
                                    1,
                                )));
                            }
                            Err(err) => {
                                actions.push(Some(Action::TimedStatusLine(err, 5)));
                            }
                        },
                        "r" | "read" => match rest.parse::<ReadMemoryExpr>() {
                            Ok(expr) => match state.read_memory(&expr) {
                                Ok(result) => actions.push(Some(Action::StatusLine(result))),
                                Err(err) => actions.push(Some(Action::TimedStatusLine(err, 5))),
                            },
                            Err(err) => actions.push(Some(Action::TimedStatusLine(err, 5))),
                        },
                        "vars" | "variables" | "locals" => {
                            let show_all = rest.trim() == "all";
                            let result = state.format_variables(show_all);
                            actions.push(Some(Action::StatusLine(result)));
                        }
                        _ => {
                            log::debug!("unknown command with arguments: '{cmd} {args}'");
                            actions.push(Some(Action::TimedStatusLine("unknown command".into(), 1)))
                        }
                    },
                    None => match args.trim() {
                        "q" | "quit" => actions.push(Some(Action::Quit)),
                        "r" | "reload" | "restart" => {
                            actions.push(Some(Action::Reload));
                        }
                        "nl" | "next-line" | "nextline" => {
                            if state.stopped && !state.executor().stopped {
                                state.create_breakpoint(BreakpointType::NextLine);
                                state.stopped = false;
                                actions.push(Some(Action::Continue));
                            } else if state.executor().stopped {
                                actions.push(Some(Action::TimedStatusLine(
                                    "program has terminated, cannot continue".to_string(),
                                    3,
                                )));
                            }
                        }
                        "debug" => {
                            actions.push(Some(Action::ShowDebug));
                        }
                        "vars" | "variables" | "locals" => {
                            let result = state.format_variables(false);
                            actions.push(Some(Action::StatusLine(result)));
                        }
                        "p" | "proc" | "where" => {
                            // Show the current procedure name so users can craft
                            // `b in <pattern>` breakpoints.
                            let live = state
                                .executor()
                                .current_asmop
                                .as_ref()
                                .map(|op| op.context_name().to_string());
                            let frame = state
                                .executor()
                                .callstack
                                .current_frame()
                                .and_then(|f| f.procedure(""))
                                .map(|p| p.to_string());
                            let msg = match (live, frame) {
                                (Some(l), Some(f)) if l == f => {
                                    format!("proc: {l}")
                                }
                                (Some(l), Some(f)) => {
                                    format!("proc (live): {l} / (frame): {f}")
                                }
                                (Some(l), None) => format!("proc (live): {l}"),
                                (None, Some(f)) => format!("proc (frame): {f}"),
                                (None, None) => "proc: <unknown>".to_string(),
                            };
                            actions.push(Some(Action::StatusLine(msg)));
                        }
                        invalid => {
                            log::debug!("unknown command: '{invalid}'");
                            actions.push(Some(Action::TimedStatusLine("unknown command".into(), 1)))
                        }
                    },
                }
            }
            Action::FooterResult(_cmd, None) => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(Action::Focus, state)?);
                }
            }
            Action::Continue => {
                // DAP client mode: send step commands over the network
                #[cfg(feature = "dap")]
                if state.debug_mode == crate::ui::state::DebugMode::Remote {
                    self.step_remote(state, &mut actions)?;
                    // Send any pending actions
                    if let Some(tx) = &mut self.command_tx {
                        actions.into_iter().flatten().for_each(|action| {
                            tx.send(action).ok();
                        });
                    }
                    return Ok(None);
                }

                // If the program has already terminated, there's nothing to run.
                // Let the user know they can restart with `:r` / `:reload`.
                if state.executor().stopped {
                    actions.push(Some(Action::TimedStatusLine(
                        "program has terminated — use :r to restart".into(),
                        3,
                    )));
                    return Ok(None);
                }

                state.run_until_stopped();

                // Report program termination to the user
                if state.stopped && state.executor().stopped {
                    if let Some(err) = state.execution_failed() {
                        actions.push(Some(Action::Error(err.to_string())));
                    } else {
                        let status = match state.typed_result() {
                            Ok(Some(result)) => {
                                format!("program terminated successfully: {result}")
                            }
                            Ok(None) => "program terminated successfully".to_string(),
                            Err(err) => format!("program terminated successfully; {err}"),
                        };
                        actions.push(Some(Action::StatusLine(status)));
                    }
                }

                // Update the UI with latest state
                for pane in self.panes.iter_mut() {
                    actions.push(pane.update(Action::Update, state)?);
                }
            }
            Action::Reload => match state.reload() {
                Ok(_) => {
                    for pane in self.panes.iter_mut() {
                        actions.push(pane.update(Action::Reload, state)?);
                    }
                }
                Err(err) => {
                    actions.push(Some(Action::TimedStatusLine(err.to_string(), 5)));
                }
            },
            _ => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane_index) {
                    actions.push(pane.update(action, state)?);
                }
            }
        }

        if let Some(tx) = &mut self.command_tx {
            actions.into_iter().flatten().for_each(|action| {
                tx.send(action).ok();
            });
        }
        Ok(None)
    }

    fn handle_key_events(
        &mut self,
        key: KeyEvent,
        state: &mut State,
    ) -> Result<Option<EventResponse<Action>>, Report> {
        match state.input_mode {
            InputMode::Normal => {
                let response = match key.code {
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => {
                        EventResponse::Stop(Action::FocusNext)
                    }
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H') => {
                        EventResponse::Stop(Action::FocusPrev)
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                        EventResponse::Stop(Action::Down)
                    }
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                        EventResponse::Stop(Action::Up)
                    }
                    KeyCode::Char('g') | KeyCode::Char('G') => EventResponse::Stop(Action::Go),
                    KeyCode::Backspace | KeyCode::Char('b') | KeyCode::Char('B') => {
                        EventResponse::Stop(Action::Back)
                    }
                    KeyCode::Char('f') | KeyCode::Char('F') => {
                        EventResponse::Stop(Action::ToggleFullScreen)
                    }
                    KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                        EventResponse::Stop(Action::Tab(c.to_digit(10).unwrap_or(0) - 1))
                    }
                    KeyCode::Char(']') => EventResponse::Stop(Action::TabNext),
                    KeyCode::Char('[') => EventResponse::Stop(Action::TabPrev),
                    KeyCode::Char(':') => {
                        EventResponse::Stop(Action::FocusFooter(":".into(), None))
                    }
                    KeyCode::Char('q') => EventResponse::Stop(Action::Quit),
                    KeyCode::Char('e') => {
                        state.create_breakpoint(BreakpointType::Finish);
                        state.stopped = false;
                        EventResponse::Stop(Action::Continue)
                    }
                    // Only step if we're stopped, and execution has not terminated
                    KeyCode::Char('s') if state.stopped && !state.executor().stopped => {
                        state.create_breakpoint(BreakpointType::Step);
                        state.stopped = false;
                        EventResponse::Stop(Action::Continue)
                    }
                    // Only step-next if we're stopped, and execution has not terminated
                    KeyCode::Char('n') if state.stopped && !state.executor().stopped => {
                        state.create_breakpoint(BreakpointType::Next);
                        state.stopped = false;
                        EventResponse::Stop(Action::Continue)
                    }
                    // Only resume execution if we're stopped, and execution has not terminated
                    KeyCode::Char('c') if state.stopped && !state.executor().stopped => {
                        state.stopped = false;
                        EventResponse::Stop(Action::Continue)
                    }
                    // Do not try to continue if execution has terminated, but warn user
                    KeyCode::Char('c' | 's' | 'n') if state.stopped && state.executor().stopped => {
                        EventResponse::Stop(Action::TimedStatusLine(
                            "program has terminated, cannot continue".to_string(),
                            3,
                        ))
                    }
                    KeyCode::Char('d') => EventResponse::Stop(Action::Delete),
                    _ => {
                        return Ok(None);
                    }
                };
                Ok(Some(response))
            }
            InputMode::Insert => Ok(None),
            InputMode::Command => Ok(None),
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, state: &State) -> Result<(), Report> {
        if let Some(fullscreen_pane_index) = self.fullscreen_pane_index {
            self.panes[fullscreen_pane_index].draw(frame, area, state)?;
        } else {
            let outer_layout = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(vec![Constraint::Fill(3), Constraint::Fill(1)])
                .split(area);

            let left_panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![
                    self.panes[0].height_constraint(),
                    self.panes[1].height_constraint(),
                    self.panes[2].height_constraint(),
                ])
                .split(outer_layout[0]);

            let right_panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![
                    self.panes[3].height_constraint(),
                    self.panes[4].height_constraint(),
                ])
                .split(outer_layout[1]);
            self.panes[0].draw(frame, left_panes[0], state)?;
            self.panes[1].draw(frame, left_panes[1], state)?;
            self.panes[2].draw(frame, left_panes[2], state)?;
            self.panes[3].draw(frame, right_panes[0], state)?;
            self.panes[4].draw(frame, right_panes[1], state)?;
        }
        Ok(())
    }
}

#[cfg(feature = "dap")]
impl Home {
    /// Handle stepping in DAP remote mode.
    ///
    /// Determines which DAP command to send based on the current breakpoints,
    /// sends it, waits for the response, and updates the TUI state.
    fn step_remote(
        &mut self,
        state: &mut State,
        actions: &mut Vec<Option<Action>>,
    ) -> Result<(), Report> {
        use crate::exec::DapStopReason;
        let result = state.step_remote();

        match result {
            Ok(DapStopReason::Stopped(_)) => state.stopped = true,
            Ok(DapStopReason::Terminated) => {
                state.executor_mut().stopped = true;
                state.stopped = true;
                actions.push(Some(Action::StatusLine("program terminated successfully".into())));
            }
            Ok(DapStopReason::Restarting) => {
                state.stopped = true;
                actions.push(Some(Action::StatusLine(
                    "server signaled Phase 2 restart during step".into(),
                )));
            }
            Err(e) => {
                state.executor_mut().stopped = true;
                state.stopped = true;
                actions.push(Some(Action::StatusLine(format!("DAP error: {e}"))));
            }
        }

        // Update panes with latest state
        for pane in self.panes.iter_mut() {
            actions.push(pane.update(Action::Update, state)?);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::string::String;

    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyModifiers};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::{DebuggerConfig, program_loader::test_package_input, ui::tui::Event};

    fn setup() -> (Home, State, UnboundedReceiver<Action>) {
        let state = State::new(Box::new(DebuggerConfig {
            input: Some(test_package_input()),
            ..Default::default()
        }))
        .unwrap();
        let mut home = Home::new().unwrap();
        let (sender, receiver) = unbounded_channel();
        home.register_action_handler(sender).unwrap();
        home.init(&state).unwrap();
        (home, state, receiver)
    }

    fn draw(home: &mut Home, state: &State) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 80)).unwrap();
        terminal.draw(|frame| home.draw(frame, frame.area(), state).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn command(
        home: &mut Home,
        state: &mut State,
        receiver: &mut UnboundedReceiver<Action>,
        text: &str,
    ) -> Action {
        home.update(Action::FooterResult(":".into(), Some(text.into())), state).unwrap();
        receiver.try_recv().unwrap()
    }

    #[test]
    fn focus_navigation_wraps_and_fullscreen_renders_only_selected_pane() {
        let (mut home, mut state, mut receiver) = setup();
        home.focus().unwrap();
        assert!(
            matches!(receiver.try_recv().unwrap(), Action::StatusLine(message) if message.contains("pane movement"))
        );
        assert_eq!(home.focused_pane_index, 0);
        home.update(Action::FocusPrev, &mut state).unwrap();
        assert_eq!(home.focused_pane_index, 4);
        home.update(Action::FocusNext, &mut state).unwrap();
        assert_eq!(home.focused_pane_index, 0);
        let normal = draw(&mut home, &state);
        assert!(normal.contains("Operand Stack"));
        assert!(normal.contains("Breakpoints"), "{normal}");
        for _ in 0..3 {
            home.update(Action::FocusNext, &mut state).unwrap();
        }
        assert_eq!(home.focused_pane_index, 3);
        home.update(Action::ToggleFullScreen, &mut state).unwrap();
        assert_eq!(home.fullscreen_pane_index, Some(3));
        let fullscreen = draw(&mut home, &state);
        assert!(fullscreen.contains("Operand Stack"));
        assert!(!fullscreen.contains("Breakpoints"));
        home.update(Action::ToggleFullScreen, &mut state).unwrap();
        assert_eq!(home.fullscreen_pane_index, None);
        assert!(draw(&mut home, &state).contains("Breakpoints"));
        home.update(Action::FocusFooter(":".into(), None), &mut state).unwrap();
        home.update(Action::FooterResult(":".into(), None), &mut state).unwrap();
        home.update(Action::Tick, &mut state).unwrap();
        home.update(Action::Update, &mut state).unwrap();
    }

    #[test]
    fn footer_commands_manage_breakpoints_and_report_parse_errors() {
        let (mut home, mut state, mut receiver) = setup();
        assert_eq!(
            command(&mut home, &mut state, &mut receiver, "b at 3"),
            Action::TimedStatusLine("breakpoint created".into(), 1)
        );
        assert_eq!(state.breakpoints.len(), 1);
        assert_eq!(state.breakpoints[0].ty, BreakpointType::StepTo(3));
        assert!(
            matches!(command(&mut home, &mut state, &mut receiver, "b at invalid"), Action::TimedStatusLine(message, 5) if message.contains("invalid breakpoint"))
        );
        assert_eq!(state.breakpoints.len(), 1);
        for text in ["unknown", "unknown argument"] {
            assert_eq!(
                command(&mut home, &mut state, &mut receiver, text),
                Action::TimedStatusLine("unknown command".into(), 1)
            );
        }
        for text in ["vars", "variables all", "locals"] {
            assert!(
                matches!(command(&mut home, &mut state, &mut receiver, text), Action::StatusLine(message) if message.contains("No debug variables tracked"))
            );
        }
        assert_eq!(
            command(&mut home, &mut state, &mut receiver, "where"),
            Action::StatusLine("proc: <unknown>".into())
        );
        assert_eq!(command(&mut home, &mut state, &mut receiver, "debug"), Action::ShowDebug);
        assert_eq!(command(&mut home, &mut state, &mut receiver, "reload"), Action::Reload);
        assert_eq!(command(&mut home, &mut state, &mut receiver, "q"), Action::Quit);
        assert!(matches!(
            command(&mut home, &mut state, &mut receiver, "r invalid"),
            Action::TimedStatusLine(_, 5)
        ));
        assert_eq!(
            command(&mut home, &mut state, &mut receiver, "r 0 -t u32"),
            Action::StatusLine("0".into())
        );
        assert!(
            matches!(command(&mut home, &mut state, &mut receiver, "r 0 -c 2"), Action::TimedStatusLine(message, 5) if message.contains("not yet implemented"))
        );
    }

    #[test]
    fn key_events_dispatch_navigation_and_respect_input_mode() {
        let (mut home, mut state, _receiver) = setup();
        for (key, action) in [
            (KeyCode::Right, Action::FocusNext),
            (KeyCode::Left, Action::FocusPrev),
            (KeyCode::Down, Action::Down),
            (KeyCode::Up, Action::Up),
            (KeyCode::Char('g'), Action::Go),
            (KeyCode::Backspace, Action::Back),
            (KeyCode::Char('f'), Action::ToggleFullScreen),
            (KeyCode::Char('2'), Action::Tab(1)),
            (KeyCode::Char(']'), Action::TabNext),
            (KeyCode::Char('['), Action::TabPrev),
            (KeyCode::Char(':'), Action::FocusFooter(":".into(), None)),
            (KeyCode::Char('q'), Action::Quit),
            (KeyCode::Char('d'), Action::Delete),
        ] {
            let response = home
                .handle_events(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut state)
                .unwrap();
            assert!(matches!(response, Some(EventResponse::Stop(actual)) if actual == action));
        }
        assert!(home.handle_events(Event::Tick, &mut state).unwrap().is_none());
        assert!(
            home.handle_key_events(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), &mut state)
                .unwrap()
                .is_none()
        );
        for mode in [InputMode::Insert, InputMode::Command] {
            state.input_mode = mode;
            assert!(
                home.handle_key_events(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                    &mut state
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn execution_keys_step_continue_and_reject_completed_programs() {
        for key in ['s', 'n', 'c', 'e'] {
            let (mut home, mut state, mut receiver) = setup();
            let response = home
                .handle_key_events(
                    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                    &mut state,
                )
                .unwrap();
            assert!(matches!(response, Some(EventResponse::Stop(Action::Continue))));
            assert!(!state.stopped);
            home.update(Action::Continue, &mut state).unwrap();
            assert!(state.executor().cycle > 0);
            home.update(Action::Reload, &mut state).unwrap();
            assert_eq!(state.executor().cycle, 0);
            while receiver.try_recv().is_ok() {}
        }
        let (mut home, mut state, mut receiver) = setup();
        assert_eq!(command(&mut home, &mut state, &mut receiver, "nl"), Action::Continue);
        home.update(Action::Continue, &mut state).unwrap();
        state.breakpoints.clear();
        home.update(Action::Continue, &mut state).unwrap();
        assert!(state.executor().stopped);
        while receiver.try_recv().is_ok() {}
        for key in ['s', 'n', 'c'] {
            let response = home
                .handle_key_events(
                    KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                    &mut state,
                )
                .unwrap();
            assert!(
                matches!(response, Some(EventResponse::Stop(Action::TimedStatusLine(message, 3))) if message.contains("terminated"))
            );
        }
        assert!(
            matches!(command(&mut home, &mut state, &mut receiver, "nl"), Action::TimedStatusLine(message, 3) if message.contains("terminated"))
        );
    }
}
