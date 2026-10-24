//! A line of terminal output that is rewritten in place.

use std::io::{self, IsTerminal, Write};

/// One line that is rewritten in place on a terminal, like the update
/// progress.
pub struct Live {
    terminal: bool,
    width: Option<usize>,
    last: String,
}

impl Live {
    pub fn new() -> Self {
        Self {
            terminal: io::stdout().is_terminal(),
            width: None,
            last: String::new(),
        }
    }

    pub fn show(&mut self, text: &str) {
        if text == self.last {
            return;
        }
        self.last = text.to_owned();
        if self.terminal {
            let width = self.width.unwrap_or(0);
            print!("\r{text:<width$}");
            io::stdout().flush().ok();
            self.width = Some(text.chars().count());
        } else {
            println!("{text}");
        }
    }

    /// Replace the line with its final text and move on.
    pub fn done(&mut self, text: &str) {
        if self.terminal {
            let width = self.width.take().unwrap_or(0);
            println!("\r{text:<width$}");
        } else if text != self.last {
            println!("{text}");
        }
        self.last.clear();
    }
}
