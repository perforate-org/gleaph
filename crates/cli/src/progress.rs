//! One-line progress rendering shared by `load` and `migration apply`.
//!
//! On a terminal the current line is rewritten in place with `\r`; when output is captured, a
//! new line is printed only when the rendered fill percent advances, so logs stay readable.

use std::io::{self, Write};

/// A 20-column progress bar for a fill percent in `0..=100`.
pub fn bar(fill_percent: u8) -> String {
    const BAR_WIDTH: usize = 20;
    let filled = (fill_percent as usize * BAR_WIDTH) / 100;
    if fill_percent >= 100 {
        "=".repeat(BAR_WIDTH)
    } else if filled == 0 {
        " ".repeat(BAR_WIDTH)
    } else {
        format!(
            "{}{}{}",
            "=".repeat(filled - 1),
            ">",
            " ".repeat(BAR_WIDTH - filled)
        )
    }
}

/// Live one-line progress.
///
/// On a terminal the line is rewritten in place via `\r` whenever the frame changes; when the
/// output is captured, a new line is printed only when the fill percent advances. Dropping an
/// unclosed line terminates it, so a later error message is not appended to the bar.
pub struct ProgressLine {
    tty: bool,
    rendered_percent: Option<u8>,
    rendered_text: Option<String>,
    open: bool,
}

impl ProgressLine {
    pub fn new(tty: bool) -> Self {
        Self {
            tty,
            rendered_percent: None,
            rendered_text: None,
            open: false,
        }
    }

    /// Render one frame: `text` with a fill of `percent` percent.
    pub fn render(&mut self, percent: u8, text: &str) {
        if self.rendered_percent == Some(percent)
            && (!self.tty || self.rendered_text.as_deref() == Some(text))
        {
            return;
        }
        if self.tty {
            print!("\r{text}");
            let _ = io::stdout().flush();
            self.open = true;
        } else {
            println!("{text}");
        }
        self.rendered_percent = Some(percent);
        self.rendered_text = Some(text.to_owned());
    }

    /// Forget the last rendered frame so the next `render` is treated as new. Used when the
    /// rendered line switches to a distinct unit of work whose first frame could otherwise be
    /// deduplicated against the previous unit's last frame.
    pub fn reset(&mut self) {
        self.rendered_percent = None;
        self.rendered_text = None;
    }

    /// Terminate the in-place terminal line. Idempotent; a no-op when captured.
    pub fn close(&mut self) {
        if self.open {
            println!();
            self.open = false;
        }
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.close();
    }
}
