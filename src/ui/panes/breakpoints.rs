use std::vec::Vec;

use miden_assembly_syntax::diagnostics::Report;
use ratatui::{prelude::*, widgets::*};

use crate::{
    debug::{Breakpoint, BreakpointType},
    ui::{action::Action, panes::Pane, state::State, tui::Frame},
};

pub struct BreakpointsPane {
    focused: bool,
    focused_border_style: Style,
    breakpoint_selected: Option<u8>,
    breakpoints_hit: Vec<Breakpoint>,
    breakpoint_cycle: usize,
}

impl BreakpointsPane {
    pub fn new(focused: bool, focused_border_style: Style) -> Self {
        Self {
            focused,
            focused_border_style,
            breakpoint_selected: None,
            breakpoints_hit: vec![],
            breakpoint_cycle: 0,
        }
    }

    fn border_style(&self) -> Style {
        match self.focused {
            true => self.focused_border_style,
            false => Style::default(),
        }
    }

    fn border_type(&self) -> BorderType {
        match self.focused {
            true => BorderType::Thick,
            false => BorderType::Plain,
        }
    }
}

impl Pane for BreakpointsPane {
    fn height_constraint(&self) -> Constraint {
        match self.focused {
            true => Constraint::Fill(5),
            false => Constraint::Fill(5),
        }
    }

    fn init(&mut self, state: &State) -> Result<(), Report> {
        self.breakpoint_cycle = state.executor().cycle;
        self.breakpoints_hit.clear();
        self.breakpoint_selected = None;
        Ok(())
    }

    fn update(&mut self, action: Action, state: &mut State) -> Result<Option<Action>, Report> {
        match action {
            Action::Focus => {
                self.focused = true;
            }
            Action::UnFocus => {
                self.focused = false;
            }
            Action::Down => {
                if let Some(prev) = self.breakpoint_selected.take() {
                    self.breakpoint_selected = state
                        .breakpoints
                        .iter()
                        .find_map(|bp| if bp.id > prev { Some(bp.id) } else { None })
                        .or_else(|| state.breakpoints.first().map(|bp| bp.id));
                } else {
                    self.breakpoint_selected = state.breakpoints.first().map(|bp| bp.id);
                }
                return Ok(Some(Action::Update));
            }
            Action::Up => {
                if let Some(prev) = self.breakpoint_selected.take() {
                    self.breakpoint_selected = state
                        .breakpoints
                        .iter()
                        .rev()
                        .find_map(|bp| if bp.id < prev { Some(bp.id) } else { None })
                        .or_else(|| state.breakpoints.last().map(|bp| bp.id));
                } else {
                    self.breakpoint_selected = state.breakpoints.last().map(|bp| bp.id);
                }
                return Ok(Some(Action::Update));
            }
            Action::Delete => {
                if let Some(prev) = self.breakpoint_selected.take() {
                    state.breakpoints.retain(|bp| bp.id != prev);
                    let select_next = state
                        .breakpoints
                        .iter()
                        .find_map(|bp| if bp.id > prev { Some(bp.id) } else { None })
                        .or_else(|| state.breakpoints.first().map(|bp| bp.id));
                    self.breakpoint_selected = select_next;
                }
            }
            Action::Reload => {
                self.init(state)?;
            }
            Action::Update => {
                if self.breakpoint_cycle < state.executor().cycle {
                    self.breakpoints_hit.clear();
                    self.breakpoints_hit.append(&mut state.breakpoints_hit);
                    if let Some(prev) = self.breakpoint_selected
                        && self.breakpoints_hit.iter().any(|bp| bp.id == prev && bp.is_one_shot())
                    {
                        self.breakpoint_selected = None;
                    }
                }
                self.breakpoint_cycle = state.executor().cycle;
            }
            _ => {}
        }

        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, state: &State) -> Result<(), Report> {
        let mut breakpoints = self
            .breakpoints_hit
            .iter()
            .map(|bp| (true, bp))
            .chain(state.breakpoints.iter().filter_map(|bp| {
                if self.breakpoints_hit.iter().any(|hit| hit.id == bp.id) {
                    None
                } else {
                    Some((false, bp))
                }
            }))
            .filter(|(_, bp)| !bp.is_internal())
            .collect::<Vec<_>>();
        breakpoints.sort_by_key(|(_, bp)| bp.id);
        let user_created_breakpoints = breakpoints.len();
        let user_breakpoints_hit =
            self.breakpoints_hit.iter().filter(|bp| !bp.is_internal()).count();

        let fg = Color::Gray;
        let bg = Color::Black;
        let yellow = Color::Yellow;
        let gray = Color::Gray;
        let fg_hit = Color::Red;
        let bg_hit = Color::Black;
        let yellow_hit = Color::LightRed;
        let gray_hit = Color::DarkGray;
        let selected_index = if let Some(id) = self.breakpoint_selected {
            breakpoints.iter().position(|(_, bp)| bp.id == id)
        } else {
            None
        };
        let lines = breakpoints
            .into_iter()
            .map(|(is_hit, bp)| {
                let (_fg, bg, gray, yellow) = if is_hit {
                    (fg_hit, bg_hit, gray_hit, yellow_hit)
                } else {
                    (fg, bg, gray, yellow)
                };
                let yellow = Style::default().fg(yellow).bg(bg);
                let gray = Style::default().fg(gray).bg(bg);
                let gutter = if is_hit {
                    Span::styled("! ", Color::Red)
                } else {
                    Span::styled("", Style::default())
                };
                let line = match &bp.ty {
                    BreakpointType::Next
                    | BreakpointType::NextLine
                    | BreakpointType::Step
                    | BreakpointType::Finish => unreachable!(),
                    BreakpointType::StepN(n) => Line::from(vec![
                        gutter,
                        Span::styled("cycle:", yellow),
                        Span::styled(format!("{}", bp.creation_cycle + *n), gray),
                    ]),
                    BreakpointType::StepTo(cycle) => Line::from(vec![
                        gutter,
                        Span::styled("cycle:", yellow),
                        Span::styled(format!("{cycle}"), gray),
                    ]),
                    BreakpointType::File(pattern) => Line::from(vec![
                        gutter,
                        Span::styled("file:", yellow),
                        Span::styled(pattern.glob().glob(), gray),
                    ]),
                    BreakpointType::Line { pattern, line } => Line::from(vec![
                        gutter,
                        Span::styled("file:", yellow),
                        Span::styled(pattern.glob().glob(), gray),
                        Span::styled(format!(":{line}"), yellow),
                    ]),
                    BreakpointType::Called(pattern) => Line::from(vec![
                        gutter,
                        Span::styled("proc:", yellow),
                        Span::styled(pattern.glob().glob(), gray),
                    ]),
                    BreakpointType::Opcode(matcher) => Line::from(vec![
                        gutter,
                        Span::styled("opcode:", yellow),
                        Span::styled(format!("{matcher}"), gray),
                    ]),
                    BreakpointType::Event(id) => Line::from(vec![
                        gutter,
                        Span::styled("event:", yellow),
                        Span::styled(format!("{id}"), gray),
                    ]),
                };
                if is_hit {
                    line.patch_style(Style::default().add_modifier(Modifier::BOLD))
                } else {
                    line
                }
            })
            .collect::<Vec<_>>();

        let list = List::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .highlight_symbol(symbols::scrollbar::HORIZONTAL.end)
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));
        let mut list_state = ListState::default().with_selected(selected_index);

        let pane = Block::default()
            .title("Breakpoints")
            .borders(Borders::ALL)
            .border_style(self.border_style())
            .border_type(self.border_type());
        let pane = if user_breakpoints_hit > 0 {
            pane.title_bottom(
                Line::styled(
                    format!(
                        " {user_breakpoints_hit} of {user_created_breakpoints} hit this cycle",
                    ),
                    Style::default().add_modifier(Modifier::ITALIC),
                )
                .right_aligned(),
            )
        } else {
            pane
        };

        frame.render_stateful_widget(list, area, &mut list_state);
        frame.render_widget(pane, area);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{boxed::Box, string::String};

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{config::DebuggerConfig, program_loader::test_package_input};

    #[test]
    fn navigation_wraps_deletion_selects_the_next_breakpoint_and_reload_clears_hits() {
        let mut state = State::new(Box::new(DebuggerConfig {
            input: Some(test_package_input()),
            ..Default::default()
        }))
        .unwrap();
        let mut pane = BreakpointsPane::new(false, Style::new().fg(Color::Blue));
        pane.init(&state).unwrap();
        assert_eq!(pane.height_constraint(), Constraint::Fill(5));
        pane.update(Action::Down, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, None);
        for breakpoint in
            ["at 10", "after 20", "in entrypoint", "for add", "main.masm:4", "main.masm"]
        {
            state.create_breakpoint(breakpoint.parse().unwrap());
        }
        let first = state.breakpoints[0].id;
        let last = state.breakpoints.last().unwrap().id;
        pane.update(Action::Up, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, Some(last));
        pane.update(Action::Down, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, Some(first));
        pane.update(Action::Up, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, Some(last));
        pane.update(Action::Delete, &mut state).unwrap();
        assert!(!state.breakpoints.iter().any(|breakpoint| breakpoint.id == last));
        assert_eq!(pane.breakpoint_selected, Some(first));
        pane.update(Action::Down, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, Some(state.breakpoints[1].id));
        pane.update(Action::Up, &mut state).unwrap();
        assert_eq!(pane.breakpoint_selected, Some(first));
        state.breakpoints_hit.push(state.breakpoints[0].clone());
        state.executor_mut().cycle += 1;
        pane.update(Action::Update, &mut state).unwrap();
        assert_eq!(pane.breakpoints_hit.len(), 1);
        assert!(state.breakpoints_hit.is_empty());
        assert_eq!(pane.breakpoint_selected, None);
        let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
        for action in [Action::Focus, Action::UnFocus] {
            pane.update(action, &mut state).unwrap();
            terminal.draw(|frame| pane.draw(frame, frame.area(), &state).unwrap()).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Breakpoints"));
            assert!(text.contains("cycle:10"));
            assert!(text.contains("opcode:add"));
            assert!(text.contains("proc:"));
            assert!(text.contains("file:"));
            assert!(text.contains("1 of 5 hit this cycle"));
        }
        pane.update(Action::Reload, &mut state).unwrap();
        assert!(pane.breakpoints_hit.is_empty());
        assert_eq!(pane.breakpoint_selected, None);
        pane.update(Action::Delete, &mut state).unwrap();
        assert_eq!(state.breakpoints.len(), 5);
    }
}
