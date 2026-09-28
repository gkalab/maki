//! The folder-trust question, drawn once before the main UI opens.
//!
//! Not an `App` overlay: it runs while the process is still single-threaded, so
//! a grant can load the project environment through `set_var` before anything
//! long-lived exists. It renders in normal flow rather than the alternate
//! screen, so the main UI opens over it and a declined question stays in
//! scrollback.

use std::io::{self, Write};
use std::process::exit;

use maki_config::project::{TRUST_DOCS, TrustAnswer, TrustQuestion, trust_question_lines};
use ratatui::backend::{Backend, TerminaBackend};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use termina::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use termina::{EventReader, PlatformTerminal, Terminal as _};

use crate::components::{hint_line, is_ctrl};
use crate::theme;

const TITLE: &str = " Folder Trust ";
const HINTS: &[(&str, &str)] = &[
    ("t/y", "Trust"),
    ("n", "Not now"),
    ("↑/↓", "Select"),
    ("Enter", "Confirm"),
];
/// Ctrl-C at the old stderr prompt killed maki, and continuing into a
/// restricted UI would only make the user quit twice.
const INTERRUPTED_EXIT_CODE: i32 = 130;
const SELECTED_MARKER: &str = "> ";
const UNSELECTED_MARKER: &str = "  ";
const INDENT: &str = "  ";
const BORDER_LINES: u16 = 2;
const FALLBACK_WIDTH: u16 = 80;

struct Choice {
    answer: TrustAnswer,
    label: &'static str,
    effect: &'static str,
}

/// "Not now" sits in the middle and is preselected, so Enter and Esc are both
/// safe. "Never" is reachable only by selecting it: a permanent no should cost
/// a deliberate keystroke, not a mistyped one.
const CHOICES: &[Choice] = &[
    Choice {
        answer: TrustAnswer::Trust,
        label: "Trust this folder",
        effect: "load its shared config now and on every later start",
    },
    Choice {
        answer: TrustAnswer::NotNow,
        label: "Not now",
        effect: "stay restricted this run, ask again next start",
    },
    Choice {
        answer: TrustAnswer::Never,
        label: "Never",
        effect: "remember the no and stop asking",
    },
];
const DEFAULT_CHOICE: usize = 1;

/// Asks, and answers `NotNow` if anything at all goes wrong. There is no path
/// through this module that yields `Trust` without a keypress that says so.
pub fn ask_trust(question: &TrustQuestion) -> TrustAnswer {
    let mut card = TrustCard::new(question);
    match draw_and_read(&mut card) {
        Ok(answer) => answer,
        Err(error) => {
            tracing::warn!(%error, "could not draw the folder trust card, staying restricted");
            TrustAnswer::NotNow
        }
    }
}

struct TrustCard {
    body: Vec<String>,
    selected: usize,
}

impl TrustCard {
    fn new(question: &TrustQuestion) -> Self {
        Self {
            body: trust_question_lines(question),
            selected: DEFAULT_CHOICE,
        }
    }

    /// Pure, so the key handling is testable without a terminal. Ctrl-C is
    /// deliberately not an answer; the caller turns it into an exit.
    fn handle_key(&mut self, key: KeyEvent) -> Option<TrustAnswer> {
        match key.code {
            KeyCode::Char('t' | 'T' | 'y' | 'Y') => Some(TrustAnswer::Trust),
            KeyCode::Char('n' | 'N') | KeyCode::Escape => Some(TrustAnswer::NotNow),
            KeyCode::Enter => Some(CHOICES[self.selected].answer),
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = (self.selected + CHOICES.len() - 1) % CHOICES.len();
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % CHOICES.len();
                None
            }
            _ => None,
        }
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let theme = theme::current();
        let mut lines = vec![Line::raw("")];
        lines.extend(self.body.iter().map(|line| {
            let text = format!("{INDENT}{line}");
            if line == TRUST_DOCS {
                Line::styled(text, theme.item_desc)
            } else {
                Line::raw(text)
            }
        }));
        lines.push(Line::raw(""));
        for (index, choice) in CHOICES.iter().enumerate() {
            let selected = index == self.selected;
            let marker = if selected {
                SELECTED_MARKER
            } else {
                UNSELECTED_MARKER
            };
            let label = if selected {
                theme.item_selected
            } else {
                theme.item
            };
            lines.push(Line::from(vec![
                Span::raw(INDENT),
                Span::styled(marker, label),
                Span::styled(choice.label, label),
                Span::styled(format!(" — {}", choice.effect), theme.item_desc),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(hint_line(HINTS));
        lines
    }

    fn height(&self, width: u16) -> u16 {
        Paragraph::new(self.lines())
            .wrap(Wrap { trim: false })
            .line_count(width.saturating_sub(BORDER_LINES)) as u16
            + BORDER_LINES
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.panel_border)
            .title(TITLE)
            .title_style(theme.panel_title)
            .style(Style::new().bg(theme.background));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(self.lines()).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn draw_and_read(card: &mut TrustCard) -> io::Result<TrustAnswer> {
    // The control handle stays out of the backend so raw/cooked mode can be
    // toggled after the backend took ownership of the drawing handle.
    let mut control = PlatformTerminal::new()?;
    let terminal = PlatformTerminal::new()?;
    let reader = terminal.event_reader();
    let width = terminal.get_dimensions().map_or(FALLBACK_WIDTH, |d| d.cols);
    let height = card.height(width);
    control.enter_raw_mode()?;
    let result = run(&mut control, terminal, reader, card, height);
    control.enter_cooked_mode().ok();
    control.flush().ok();
    result
}

fn run(
    control: &mut PlatformTerminal,
    terminal: PlatformTerminal,
    reader: EventReader,
    card: &mut TrustCard,
    height: u16,
) -> io::Result<TrustAnswer> {
    let mut ratatui = Terminal::with_options(
        TerminaBackend::new(terminal),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    ratatui.hide_cursor()?;

    let answer = loop {
        ratatui.draw(|frame| card.view(frame, frame.area()))?;
        match reader.read(|_| true)? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if is_ctrl(&key) && matches!(key.code, KeyCode::Char('c' | 'C')) {
                    ratatui.show_cursor().ok();
                    control.enter_cooked_mode().ok();
                    exit(INTERRUPTED_EXIT_CODE);
                }
                if let Some(answer) = card.handle_key(key) {
                    break answer;
                }
            }
            Event::WindowResized(..) => ratatui.autoresize()?,
            _ => {}
        }
    };

    ratatui.draw(|frame| card.view(frame, frame.area()))?;
    ratatui.show_cursor()?;
    // Leaves the answered card above the cursor, so the shell prompt after the
    // session does not land on top of it.
    ratatui.backend_mut().append_lines(height)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use maki_config::project::GatedFile;
    use maki_storage::trusted_folders::CanonicalFolder;
    use termina::event::Modifiers;
    use test_case::test_case;

    use super::*;

    const CTRL_C: char = 'c';

    fn card() -> TrustCard {
        let folder = CanonicalFolder::resolve(std::env::temp_dir().as_path()).unwrap();
        TrustCard::new(&TrustQuestion {
            folder,
            present: vec![GatedFile::InitLua],
            added: Vec::new(),
        })
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    #[test_case(KeyCode::Char('t'), TrustAnswer::Trust ; "t_trusts")]
    #[test_case(KeyCode::Char('y'), TrustAnswer::Trust ; "y_trusts")]
    #[test_case(KeyCode::Char('n'), TrustAnswer::NotNow ; "n_defers")]
    #[test_case(KeyCode::Escape, TrustAnswer::NotNow ; "esc_defers")]
    #[test_case(KeyCode::Enter, TrustAnswer::NotNow ; "enter_takes_the_safe_default")]
    fn a_key_answers_the_question(code: KeyCode, expected: TrustAnswer) {
        assert_eq!(card().handle_key(press(code)), Some(expected));
    }

    /// The permanent no costs a deliberate selection, so no single keystroke
    /// can reach it.
    #[test_case(KeyCode::Down ; "down")]
    #[test_case(KeyCode::Tab ; "tab")]
    fn never_is_reachable_only_by_selecting_it(code: KeyCode) {
        let mut card = card();

        assert_eq!(card.handle_key(press(code)), None);
        assert_eq!(
            card.handle_key(press(KeyCode::Enter)),
            Some(TrustAnswer::Never)
        );
    }

    #[test]
    fn up_from_the_default_selects_trust() {
        let mut card = card();

        assert_eq!(card.handle_key(press(KeyCode::Up)), None);
        assert_eq!(
            card.handle_key(press(KeyCode::Enter)),
            Some(TrustAnswer::Trust)
        );
    }

    /// Ctrl-C exits the process, so it must never surface as an answer the
    /// caller could record.
    #[test]
    fn ctrl_c_is_not_an_answer() {
        let key = KeyEvent::new(KeyCode::Char(CTRL_C), Modifiers::CONTROL);

        assert_eq!(card().handle_key(key), None);
    }
}
