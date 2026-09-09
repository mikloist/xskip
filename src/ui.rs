//! UI thread: ratatui alternate-screen chat view + input line.
//!
//! Reads decoded display lines from the data-plane over an SPSC ring and shows
//! them as scrollback; keystrokes build an input line that, on Enter, is turned
//! into an IRC command pushed back over a second SPSC ring. No async runtime —
//! a plain timed poll loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Text;
use ratatui::widgets::{Block, Paragraph, Wrap};
use rtrb::{Consumer, Producer};

const SCROLLBACK_MAX: usize = 4000;

struct State {
    lines: Vec<String>,
    input: String,
    nick: String,
    channel: String,
}

pub fn run(
    mut to_net: Producer<String>,
    mut from_net: Consumer<String>,
    running: Arc<AtomicBool>,
    nick: String,
    channel: String,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut state = State {
        lines: vec![
            "rustssi — type to chat, /cmd for raw IRC, Esc or Ctrl-C to quit".to_string(),
        ],
        input: String::new(),
        nick,
        channel,
    };

    let res = event_loop(&mut terminal, &mut state, &mut to_net, &mut from_net, &running);
    ratatui::restore();
    running.store(false, Ordering::SeqCst);
    res
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut State,
    to_net: &mut Producer<String>,
    from_net: &mut Consumer<String>,
    running: &AtomicBool,
) -> Result<()> {
    while running.load(Ordering::SeqCst) {
        // Drain inbound display lines.
        while let Ok(line) = from_net.pop() {
            state.lines.push(line);
            if state.lines.len() > SCROLLBACK_MAX {
                state.lines.drain(..state.lines.len() - SCROLLBACK_MAX);
            }
        }

        terminal.draw(|frame| draw(frame, state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Esc => break,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char(c) => state.input.push(c),
                    KeyCode::Backspace => {
                        state.input.pop();
                    }
                    KeyCode::Enter => submit(state, to_net),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Turn the current input into an IRC command + a local echo line.
fn submit(state: &mut State, to_net: &mut Producer<String>) {
    let cmd = build_command(&state.input, &state.nick, &state.channel);
    state.input.clear();
    let Some((wire, echo)) = cmd else {
        return;
    };
    if to_net.push(wire).is_err() {
        state.lines.push("-- send queue full, dropped --".to_string());
    }
    state.lines.push(echo);
}

/// Map raw input to `(wire IRC line, local echo)`. `/x` sends `x` raw; plain
/// text becomes a PRIVMSG to the current channel. Empty input yields `None`.
fn build_command(input: &str, nick: &str, channel: &str) -> Option<(String, String)> {
    let line = input.trim();
    if line.is_empty() {
        return None;
    }
    Some(if let Some(raw) = line.strip_prefix('/') {
        (raw.to_string(), format!("* /{raw}"))
    } else {
        (
            format!("PRIVMSG {channel} :{line}"),
            format!("<{nick}> {line}"),
        )
    })
}

fn draw(frame: &mut ratatui::Frame, state: &State) {
    let chunks =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(frame.area());

    // Show the tail that fits the messages pane.
    let height = chunks[0].height.saturating_sub(2) as usize; // account for border
    let start = state.lines.len().saturating_sub(height.max(1));
    let body = state.lines[start..].join("\n");
    let messages = Paragraph::new(Text::raw(body))
        .block(Block::bordered().title("rustssi"))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    let input = Paragraph::new(state.input.as_str())
        .block(Block::bordered().title(format!("{} @ {}", state.nick, state.channel)));
    frame.render_widget(input, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::build_command;

    #[test]
    fn plain_text_becomes_privmsg() {
        let (wire, echo) = build_command("hello there", "me", "#test").unwrap();
        assert_eq!(wire, "PRIVMSG #test :hello there");
        assert_eq!(echo, "<me> hello there");
    }

    #[test]
    fn slash_sends_raw_command() {
        let (wire, echo) = build_command("/join #other", "me", "#test").unwrap();
        assert_eq!(wire, "join #other");
        assert_eq!(echo, "* /join #other");
    }

    #[test]
    fn blank_input_is_ignored() {
        assert!(build_command("   ", "me", "#test").is_none());
        assert!(build_command("", "me", "#test").is_none());
    }
}
