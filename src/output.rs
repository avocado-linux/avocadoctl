//! Universal output management for avocadoctl
//!
//! This module provides a consistent interface for all output in the CLI,
//! handling verbosity levels and formatting consistently across all commands.

use std::io::Write;
use std::sync::mpsc::SyncSender;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

/// Output manager that handles verbosity and formatting consistently
pub struct OutputManager {
    verbose: bool,
    json: bool,
    /// When set, messages are streamed through this channel as they are produced.
    /// Used by the varlink streaming handlers for real-time progress.
    sender: Option<SyncSender<String>>,
}

impl OutputManager {
    /// Create a new output manager with the specified verbosity and format level
    pub fn new(verbose: bool, json: bool) -> Self {
        Self {
            verbose,
            json,
            sender: None,
        }
    }

    /// Create an output manager that streams messages through a channel.
    /// Each `log_info` / `log_success` call sends a message immediately.
    pub fn new_streaming(sender: SyncSender<String>) -> Self {
        Self {
            verbose: false,
            json: false,
            sender: Some(sender),
        }
    }

    /// Whether output should be machine-readable JSON
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Determine the color choice for terminal output
    fn color_choice() -> ColorChoice {
        if std::env::var("NO_COLOR").is_ok() || std::env::var("AVOCADO_TEST_MODE").is_ok() {
            ColorChoice::Never
        } else {
            ColorChoice::Auto
        }
    }

    /// Print a colored prefix with message
    fn print_colored_prefix(&self, prefix: &str, color: Color, message: &str) {
        let color_choice = Self::color_choice();

        let mut stdout = StandardStream::stdout(color_choice);
        let mut color_spec = ColorSpec::new();
        color_spec.set_fg(Some(color)).set_bold(true);

        if stdout.set_color(&color_spec).is_ok() && color_choice != ColorChoice::Never {
            let _ = write!(&mut stdout, "[{prefix}]");
            let _ = stdout.reset();
            println!(" {message}");
        } else {
            // Fallback for environments without color support
            println!("[{prefix}] {message}");
        }
    }

    /// Print a colored prefix with operation and message
    fn print_colored_prefix_with_op(
        &self,
        prefix: &str,
        color: Color,
        operation: &str,
        message: &str,
    ) {
        let color_choice = Self::color_choice();

        let mut stdout = StandardStream::stdout(color_choice);
        let mut color_spec = ColorSpec::new();
        color_spec.set_fg(Some(color)).set_bold(true);

        if stdout.set_color(&color_spec).is_ok() && color_choice != ColorChoice::Never {
            let _ = write!(&mut stdout, "[{prefix}]");
            let _ = stdout.reset();
            println!(" {operation}: {message}");
        } else {
            // Fallback for environments without color support
            println!("[{prefix}] {operation}: {message}");
        }
    }

    /// Print a success message
    /// In non-verbose mode: shows brief success
    /// In verbose mode: shows detailed success with context
    /// Suppressed in JSON mode (structured output only)
    pub fn success(&self, operation: &str, message: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            self.print_colored_prefix_with_op("SUCCESS", Color::Green, operation, message);
        } else {
            self.print_colored_prefix("SUCCESS", Color::Green, message);
        }
    }

    /// Print an error message
    /// Always shows detailed error information for developers
    pub fn error(&self, operation: &str, message: &str) {
        let color_choice = Self::color_choice();

        let mut stderr = StandardStream::stderr(color_choice);
        let mut color_spec = ColorSpec::new();
        color_spec.set_fg(Some(Color::Red)).set_bold(true);

        if stderr.set_color(&color_spec).is_ok() && color_choice != ColorChoice::Never {
            let _ = write!(&mut stderr, "[ERROR]");
            let _ = stderr.reset();
            eprintln!(" {operation}: {message}");
        } else {
            eprintln!("[ERROR] {operation}: {message}");
        }

        if !self.verbose {
            eprintln!("   Use --verbose for more details");
        }
    }

    /// Print an informational message
    /// Suppressed in JSON mode
    pub fn info(&self, operation: &str, message: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            self.print_colored_prefix_with_op("INFO", Color::Blue, operation, message);
        }
    }

    /// Print detailed progress information (verbose only, suppressed in JSON mode)
    pub fn progress(&self, message: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            println!("   {message}");
        }
    }

    /// Print a step in a process (verbose only, suppressed in JSON mode)
    pub fn step(&self, step: &str, description: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            println!("   → {step}: {description}");
        }
    }

    /// Print raw output (like command results, suppressed in JSON mode)
    pub fn raw(&self, content: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            println!("{content}");
        }
    }

    /// Get the verbosity level
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Print a status header (suppressed in JSON mode)
    pub fn status_header(&self, title: &str) {
        if self.json {
            return;
        }
        if self.verbose {
            println!("\n{title}");
            println!("{}", "=".repeat(title.len()));
            println!();
        } else {
            println!("{title}");
        }
    }

    /// Print a brief status (suppressed in JSON mode)
    pub fn status(&self, message: &str) {
        if self.json {
            return;
        }
        println!("{message}");
    }

    /// Log an informational message.
    ///
    /// In normal mode: prints to stdout with color (always, regardless of verbosity).
    /// In streaming mode: sends through channel immediately.
    /// In capture mode: captures to the message buffer for returning via varlink.
    pub fn log_info(&self, message: &str) {
        if let Some(ref tx) = self.sender {
            let _ = tx.send(format!("[INFO] {message}"));
            // Tee to stdout so the daemon's journal captures the narration even for
            // non-streaming (batch/nudge) callers that discard the reply string.
            if !self.json {
                println!("{message}");
            }
        } else if !self.json {
            self.print_colored_prefix("INFO", Color::Blue, message);
        }
    }

    /// Log a success message.
    ///
    /// In normal mode: prints to stdout with color (always, regardless of verbosity).
    /// In streaming mode: sends through channel immediately and tees to stdout.
    pub fn log_success(&self, message: &str) {
        if let Some(ref tx) = self.sender {
            let _ = tx.send(format!("[SUCCESS] {message}"));
            if !self.json {
                println!("{message}");
            }
        } else if !self.json {
            self.print_colored_prefix("SUCCESS", Color::Green, message);
        }
    }

    /// Log an error line that must reach both the streaming client and the
    /// daemon journal. Unlike [`Self::error`], this routes through the streaming
    /// channel when present so a watching operator sees the failure, and tees to
    /// stderr so `journalctl` captures it for a bystander.
    pub fn log_error(&self, message: &str) {
        if let Some(ref tx) = self.sender {
            let _ = tx.send(format!("[ERROR] {message}"));
            eprintln!("{message}");
        } else {
            eprintln!("[ERROR] {message}");
        }
    }
}
