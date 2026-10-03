use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use iced::keyboard::{self, key::Named};
use iced::widget::{column, container, text};
use iced::{Color, Element, Font, Length, Subscription, Theme};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const TERMINAL_ROWS: u16 = 34;
const TERMINAL_COLUMNS: u16 = 104;
const CELL_WIDTH: f32 = 9.0;
const CELL_HEIGHT: f32 = 18.0;
const VIEW_PADDING: f32 = 24.0;

fn main() -> iced::Result {
    iced::application(Terminal::new, Terminal::update, Terminal::view)
        .title("Termite")
        .window_size(iced::Size::new(960.0, 640.0))
        .theme(|_: &Terminal| Theme::Dark)
        .subscription(|_| {
            Subscription::batch([
                iced::time::every(Duration::from_millis(16)).map(|_| Message::Refresh),
                iced::event::listen_with(keyboard_input),
            ])
        })
        .run()
}

struct Terminal {
    parser: vt100::Parser,
    output: Option<Receiver<ReaderEvent>>,
    writer: Option<Box<dyn Write + Send>>,
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    status: Option<String>,
}

enum ReaderEvent {
    Output(Vec<u8>),
    Closed,
    Failed(String),
}

#[derive(Debug, Clone)]
enum Message {
    Input(Vec<u8>),
    Resize(u16, u16),
    Refresh,
}

impl Terminal {
    fn new() -> Self {
        let mut terminal = Self {
            parser: vt100::Parser::new(TERMINAL_ROWS, TERMINAL_COLUMNS, 2_000),
            output: None,
            writer: None,
            _master: None,
            child: None,
            status: None,
        };

        if let Err(error) = terminal.start_shell() {
            terminal.status = Some(format!("Unable to start shell: {error}"));
        }

        terminal
    }

    fn start_shell(&mut self) -> Result<(), String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: TERMINAL_ROWS,
                cols: TERMINAL_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| error.to_string())?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| error.to_string())?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| error.to_string())?;
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        let mut command = CommandBuilder::new(shell);
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| error.to_string())?;
        let (sender, receiver) = mpsc::channel();

        if let Err(error) = thread::Builder::new()
            .name("termite-pty-reader".into())
            .spawn(move || read_pty(reader, sender))
        {
            if let Err(kill_error) = child.kill() {
                eprintln!("Unable to stop shell after reader startup failed: {kill_error}");
            }
            return Err(error.to_string());
        }

        self.output = Some(receiver);
        self.writer = Some(writer);
        self._master = Some(pair.master);
        self.child = Some(child);
        Ok(())
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Input(bytes) => {
                if let Some(writer) = &mut self.writer {
                    if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                        self.status = Some(format!("Unable to write to shell: {error}"));
                    }
                }
            }
            Message::Resize(rows, columns) => self.resize(rows, columns),
            Message::Refresh => self.refresh(),
        }
    }

    fn resize(&mut self, rows: u16, columns: u16) {
        if let Some(master) = &self._master {
            if let Err(error) = master.resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                self.status = Some(format!("Unable to resize shell: {error}"));
                return;
            }
        }

        self.parser.screen_mut().set_size(rows, columns);
    }

    fn refresh(&mut self) {
        if let Some(output) = &self.output {
            loop {
                match output.try_recv() {
                    Ok(ReaderEvent::Output(bytes)) => self.parser.process(&bytes),
                    Ok(ReaderEvent::Closed) => {
                        self.status = Some("Shell session ended.".into());
                        self.output = None;
                        break;
                    }
                    Ok(ReaderEvent::Failed(error)) => {
                        self.status = Some(format!("Unable to read from shell: {error}"));
                        self.output = None;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.status = Some("Shell output channel closed unexpectedly.".into());
                        self.output = None;
                        break;
                    }
                }
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let screen = text(screen_contents(&self.parser))
            .font(Font::MONOSPACE)
            .size(15)
            .color(Color::WHITE);

        let content = if let Some(status) = &self.status {
            column![
                screen,
                text(status)
                    .font(Font::MONOSPACE)
                    .size(13)
                    .color(Color::WHITE)
            ]
        } else {
            column![screen]
        };

        container(content)
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| iced::widget::container::Style {
                text_color: Some(Color::WHITE),
                background: Some(Color::BLACK.into()),
                ..Default::default()
            })
            .into()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = child.kill() {
                        eprintln!("Unable to stop shell process: {error}");
                    }
                }
                Err(error) => eprintln!("Unable to check shell process before shutdown: {error}"),
            }
        }
    }
}

fn read_pty(mut reader: Box<dyn Read + Send>, sender: Sender<ReaderEvent>) {
    let mut buffer = [0; 4096];

    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(ReaderEvent::Closed);
                break;
            }
            Ok(length) => {
                if sender
                    .send(ReaderEvent::Output(buffer[..length].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(ReaderEvent::Failed(error.to_string()));
                break;
            }
        }
    }
}

fn keyboard_input(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    if status != iced::event::Status::Ignored {
        return None;
    }

    if let iced::Event::Window(iced::window::Event::Resized(size)) = &event {
        let columns = ((size.width - VIEW_PADDING).max(CELL_WIDTH) / CELL_WIDTH)
            .floor()
            .clamp(1.0, u16::MAX as f32) as u16;
        let rows = ((size.height - VIEW_PADDING).max(CELL_HEIGHT) / CELL_HEIGHT)
            .floor()
            .clamp(1.0, u16::MAX as f32) as u16;
        return Some(Message::Resize(rows, columns));
    }

    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        text,
        ..
    }) = event
    else {
        return None;
    };

    if modifiers.logo() {
        return None;
    }

    let bytes = if modifiers.control() {
        let control_bytes = match key.to_latin(physical_key) {
            Some(character) if character.is_ascii_alphabetic() => {
                Some(vec![character.to_ascii_lowercase() as u8 & 0x1f])
            }
            Some(' ') => Some(vec![0]),
            Some('[') => Some(vec![0x1b]),
            Some('\\') => Some(vec![0x1c]),
            Some(']') => Some(vec![0x1d]),
            Some('^') => Some(vec![0x1e]),
            Some('_') => Some(vec![0x1f]),
            _ => None,
        };
        control_bytes.or_else(|| key_bytes(&key, text.as_deref()))
    } else {
        key_bytes(&key, text.as_deref())
    }?;

    let bytes = if modifiers.alt() {
        [vec![0x1b], bytes].concat()
    } else {
        bytes
    };

    Some(Message::Input(bytes))
}

fn key_bytes(key: &keyboard::Key, text: Option<&str>) -> Option<Vec<u8>> {
    if let Some(text) = text {
        if !text.is_empty() {
            return Some(text.as_bytes().to_vec());
        }
    }

    match key {
        keyboard::Key::Named(named) => Some(match named {
            Named::Enter => b"\r".to_vec(),
            Named::Tab => b"\t".to_vec(),
            Named::Backspace => b"\x7f".to_vec(),
            Named::Escape => b"\x1b".to_vec(),
            Named::ArrowUp => b"\x1b[A".to_vec(),
            Named::ArrowDown => b"\x1b[B".to_vec(),
            Named::ArrowRight => b"\x1b[C".to_vec(),
            Named::ArrowLeft => b"\x1b[D".to_vec(),
            Named::Home => b"\x1b[H".to_vec(),
            Named::End => b"\x1b[F".to_vec(),
            Named::Insert => b"\x1b[2~".to_vec(),
            Named::Delete => b"\x1b[3~".to_vec(),
            Named::PageUp => b"\x1b[5~".to_vec(),
            Named::PageDown => b"\x1b[6~".to_vec(),
            _ => return None,
        }),
        _ => None,
    }
}

fn screen_contents(parser: &vt100::Parser) -> String {
    let screen = parser.screen();
    let (rows, columns) = screen.size();
    let cursor = screen.cursor_position();
    let mut contents = String::with_capacity(rows as usize * columns as usize);

    for row in 0..rows {
        for column in 0..columns {
            if !screen.hide_cursor() && cursor == (row, column) {
                contents.push('█');
            } else if let Some(cell) = screen.cell(row, column) {
                contents.push_str(cell.contents());
            }
        }

        if row + 1 < rows {
            contents.push('\n');
        }
    }

    contents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_shell_executes_a_command_and_renders_output() {
        let mut terminal = Terminal::new();
        assert!(
            terminal.status.is_none(),
            "shell startup failed: {:?}",
            terminal.status
        );

        terminal.update(Message::Input(b"printf TERMITE_PTY_OK\r".to_vec()));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        while std::time::Instant::now() < deadline {
            terminal.refresh();
            if screen_contents(&terminal.parser)
                .matches("TERMITE_PTY_OK")
                .count()
                >= 2
            {
                return;
            }
            thread::sleep(Duration::from_millis(16));
        }

        panic!("shell did not render command output");
    }

    #[test]
    fn terminal_keys_map_to_vt_sequences() {
        assert_eq!(
            key_bytes(&keyboard::Key::Named(Named::ArrowUp), None),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&keyboard::Key::Named(Named::Enter), None),
            Some(b"\r".to_vec())
        );
    }
}
