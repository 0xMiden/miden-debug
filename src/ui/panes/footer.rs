use std::{
    collections::VecDeque,
    string::{String, ToString},
    time::Instant,
};

use miden_assembly_syntax::diagnostics::Report;
use ratatui::{
    crossterm::event::{Event, KeyCode, KeyEvent},
    prelude::*,
    widgets::Paragraph,
};
use tui_input::{Input, backend::crossterm::EventHandler};

use crate::ui::{
    action::Action,
    panes::Pane,
    state::{InputMode, State},
    tui::{EventResponse, Frame},
};

struct TimedStatusLine {
    created: Instant,
    show_time: u64,
    status_line: String,
}

struct Config {
    max_command_history: usize,
}

static CONFIG: Config = Config {
    max_command_history: 20,
};

#[derive(Default)]
pub struct FooterPane {
    focused: bool,
    input: Input,
    command: String,
    status_line: String,
    timed_status_line: Option<TimedStatusLine>,
    command_history: VecDeque<String>,
    command_history_index: Option<usize>,
}

impl FooterPane {
    pub fn new() -> Self {
        Self {
            focused: false,
            ..Default::default()
        }
    }

    fn get_status_line(&mut self) -> &String {
        if self
            .timed_status_line
            .as_ref()
            .is_some_and(|tsl| tsl.created.elapsed().as_secs() < tsl.show_time)
        {
            return &self.timed_status_line.as_ref().unwrap().status_line;
        }
        self.timed_status_line = None;
        &self.status_line
    }
}

impl Pane for FooterPane {
    fn height_constraint(&self) -> Constraint {
        Constraint::Max(1)
    }

    fn handle_key_events(
        &mut self,
        key: KeyEvent,
        state: &mut State,
    ) -> Result<Option<EventResponse<Action>>, Report> {
        match state.input_mode {
            InputMode::Command => {
                self.input.handle_event(&Event::Key(key));
                let response = match key.code {
                    KeyCode::Enter => {
                        let command = self.input.to_string();
                        if !command.is_empty() {
                            self.command_history.push_front(self.input.to_string());
                            self.command_history.truncate(CONFIG.max_command_history);
                            self.command_history_index = None;
                        }
                        Some(EventResponse::Stop(Action::FooterResult(
                            self.command.clone(),
                            Some(command),
                        )))
                    }
                    KeyCode::Esc => {
                        self.command_history_index = None;
                        Some(EventResponse::Stop(Action::FooterResult(self.command.clone(), None)))
                    }
                    KeyCode::Up if !self.command_history.is_empty() => {
                        let history_index = self
                            .command_history_index
                            .map(|idx| idx.saturating_add(1) % self.command_history.len())
                            .unwrap_or(0);
                        self.input = self
                            .input
                            .clone()
                            .with_value(self.command_history[history_index].clone());
                        self.command_history_index = Some(history_index);
                        None
                    }
                    KeyCode::Down if !self.command_history.is_empty() => {
                        let history_index = self
                            .command_history_index
                            .map(|idx| {
                                idx.saturating_add(self.command_history.len() - 1)
                                    % self.command_history.len()
                            })
                            .unwrap_or(self.command_history.len() - 1);
                        self.input = self
                            .input
                            .clone()
                            .with_value(self.command_history[history_index].clone());
                        self.command_history_index = Some(history_index);
                        None
                    }
                    _ => None,
                };
                Ok(response)
            }
            _ => Ok(None),
        }
    }

    fn update(&mut self, action: Action, state: &mut State) -> Result<Option<Action>, Report> {
        match action {
            Action::FocusFooter(cmd, args) => {
                self.focused = true;
                state.input_mode = InputMode::Command;
                if let Some(args) = args {
                    self.input = self.input.clone().with_value(args);
                } else {
                    self.input = self.input.clone().with_value("".into());
                }
                self.command = cmd;
                Ok(Some(Action::Update))
            }
            Action::FooterResult(..) => {
                state.input_mode = InputMode::Normal;
                self.focused = false;
                Ok(Some(Action::Update))
            }
            Action::StatusLine(status_line) => {
                self.status_line = status_line;
                Ok(None)
            }
            Action::TimedStatusLine(status_line, show_time) => {
                self.timed_status_line = Some(TimedStatusLine {
                    status_line,
                    show_time,
                    created: Instant::now(),
                });
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, state: &State) -> Result<(), Report> {
        if self.focused {
            let mut area = area;
            area.width = area.width.saturating_sub(4);

            let width = area.width.max(3);
            let scroll = self.input.visual_scroll(width as usize - self.command.len());
            let input = Paragraph::new(Line::from(vec![
                Span::styled(&self.command, Style::default().fg(Color::LightBlue)),
                Span::styled(self.input.value(), Style::default()),
            ]))
            .scroll((0, scroll as u16));
            frame.render_widget(input, area);

            frame.set_cursor_position(Position::new(
                area.x
                    + ((self.input.visual_cursor()).max(scroll) - scroll) as u16
                    + self.command.len() as u16,
                area.y + 1,
            ));
        } else {
            frame.render_widget(
                Line::from(vec![Span::styled(self.get_status_line(), Style::default())])
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
        }
        frame.render_widget(
            Line::from(vec![match state.input_mode {
                InputMode::Normal => Span::from("[N]"),
                InputMode::Insert => Span::from("[I]"),
                InputMode::Command => Span::from("[C]"),
            }])
            .right_aligned(),
            area,
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{boxed::Box, time::Duration};

    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyModifiers};

    use super::*;
    use crate::{config::DebuggerConfig, program_loader::test_package_input};

    fn state() -> State {
        State::new(Box::new(DebuggerConfig {
            input: Some(test_package_input()),
            ..Default::default()
        }))
        .unwrap()
    }

    #[test]
    fn command_history_navigation_wraps_and_submission_and_cancel_restore_mode() {
        let mut state = state();
        let mut pane = FooterPane::new();
        assert_eq!(pane.height_constraint(), Constraint::Max(1));
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert!(pane.handle_key_events(key(KeyCode::Enter), &mut state).unwrap().is_none());
        for command in ["first", "second"] {
            assert_eq!(
                pane.update(
                    Action::FocusFooter(":break ".into(), Some(command.into())),
                    &mut state
                )
                .unwrap(),
                Some(Action::Update)
            );
            assert!(pane.focused);
            assert_eq!(state.input_mode, InputMode::Command);
            assert!(
                matches!(pane.handle_key_events(key(KeyCode::Enter), &mut state).unwrap(), Some(EventResponse::Stop(Action::FooterResult(_, Some(value)))) if value == command)
            );
            pane.update(Action::FooterResult(":break ".into(), None), &mut state).unwrap();
            assert!(!pane.focused);
            assert_eq!(state.input_mode, InputMode::Normal);
        }
        pane.update(Action::FocusFooter(":break ".into(), None), &mut state).unwrap();
        for (code, expected) in [
            (KeyCode::Up, "second"),
            (KeyCode::Up, "first"),
            (KeyCode::Up, "second"),
            (KeyCode::Down, "first"),
        ] {
            assert!(pane.handle_key_events(key(code), &mut state).unwrap().is_none());
            assert_eq!(pane.input.value(), expected);
        }
        assert!(matches!(
            pane.handle_key_events(key(KeyCode::Esc), &mut state).unwrap(),
            Some(EventResponse::Stop(Action::FooterResult(_, None)))
        ));
        assert_eq!(pane.command_history_index, None);
        pane.handle_key_events(key(KeyCode::Down), &mut state).unwrap();
        assert_eq!(pane.input.value(), "first");
        pane.update(Action::FocusFooter(":break ".into(), None), &mut state).unwrap();
        pane.handle_key_events(key(KeyCode::Enter), &mut state).unwrap();
        assert_eq!(pane.command_history.len(), 2);
        for index in 0..25 {
            pane.input = Input::new(index.to_string());
            pane.handle_key_events(key(KeyCode::Enter), &mut state).unwrap();
        }
        assert_eq!(pane.command_history.len(), 20);
        assert_eq!(pane.command_history.back().unwrap(), "5");
    }

    #[test]
    fn footer_renders_input_modes_and_expires_temporary_status() {
        let mut state = state();
        let mut pane = FooterPane::new();
        pane.update(Action::StatusLine("ready".into()), &mut state).unwrap();
        pane.update(Action::TimedStatusLine("temporary".into(), 10), &mut state)
            .unwrap();
        assert_eq!(pane.get_status_line(), "temporary");
        pane.timed_status_line.as_mut().unwrap().created = Instant::now() - Duration::from_secs(11);
        assert_eq!(pane.get_status_line(), "ready");
        assert!(pane.timed_status_line.is_none());
        let mut terminal = Terminal::new(TestBackend::new(60, 2)).unwrap();
        for (mode, label) in [(InputMode::Normal, "[N]"), (InputMode::Insert, "[I]")] {
            state.input_mode = mode;
            terminal
                .draw(|frame| pane.draw(frame, Rect::new(0, 0, 60, 1), &state).unwrap())
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("ready"));
            assert!(text.contains(label));
        }
        pane.update(Action::FocusFooter(":break ".into(), Some("main".into())), &mut state)
            .unwrap();
        terminal
            .draw(|frame| pane.draw(frame, Rect::new(0, 0, 60, 1), &state).unwrap())
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains(":break main"));
        assert!(text.contains("[C]"));
        assert!(pane.update(Action::Noop, &mut state).unwrap().is_none());
    }
}
