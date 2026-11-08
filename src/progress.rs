//! A `\r`-rewritable progress line, shared by the GIP and Nano update paths.

use std::io::{self, IsTerminal, Write};

pub struct ProgressLine {
    terminal: bool,
    /// Width of the line currently on screen, to blank it on the next write.
    open_line: Option<usize>,
    /// Which 25-percentage-point step was last printed when not on a
    /// terminal, so a redirected log gets occasional lines instead of one
    /// per block.
    last_step: Option<u64>,
}

impl ProgressLine {
    pub fn new() -> Self {
        Self {
            terminal: io::stdout().is_terminal(),
            open_line: None,
            last_step: None,
        }
    }

    /// Keep whatever is on the progress line and move past it before
    /// printing something else with `println!`.
    pub fn break_line(&mut self) {
        if self.open_line.take().is_some() {
            println!();
        }
    }

    /// Update the line to `text`, which should already embed `percent`
    /// (0-100). Rewrites in place on a terminal; otherwise prints only every
    /// 25 percentage points, so a redirected log is not flooded.
    pub fn update(&mut self, text: &str, percent: u64) {
        if self.terminal {
            let width = self.open_line.unwrap_or(0);
            print!("\r{text:<width$}");
            io::stdout().flush().ok();
            self.open_line = Some(text.len());
        } else if self.last_step != Some(percent / 25) {
            self.last_step = Some(percent / 25);
            println!("{text}");
        }
    }

    /// Clear the open progress line, if any, and print `text` in its place.
    pub fn finish(&mut self, text: &str) {
        if let Some(width) = self.open_line.take() {
            print!("\r{:width$}\r", "");
        }
        println!("{text}");
        self.last_step = None;
    }
}
