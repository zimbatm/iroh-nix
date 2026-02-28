//! CLI output helpers for consistent, styled terminal output
//!
//! This module provides utilities for:
//! - Styled messages (success, error, warning, info)
//! - Progress bars and spinners
//! - JSON output mode support

use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Output mode for CLI commands
#[derive(Debug, Clone, Copy, Default)]
pub enum OutputMode {
    /// Human-readable styled output (default)
    #[default]
    Human,
    /// Machine-readable JSON output
    Json,
}

/// Print a success message with green checkmark
pub fn success(msg: &str) {
    let prefix = style("[+]").green().bold();
    println!("{prefix} {msg}");
}

/// Print an error message with red X
pub fn error(msg: &str) {
    let prefix = style("[!]").red().bold();
    eprintln!("{prefix} {msg}");
}

/// Print a warning message with yellow !
pub fn warn(msg: &str) {
    let prefix = style("[!]").yellow().bold();
    eprintln!("{prefix} {msg}");
}

/// Print an info message with blue arrow
pub fn info(msg: &str) {
    let prefix = style("[*]").cyan().bold();
    println!("{prefix} {msg}");
}

/// Print a dim/secondary message (for details)
pub fn detail(msg: &str) {
    let styled = style(format!("    {msg}")).dim();
    println!("{styled}");
}

/// Style text as a key (for key: value pairs)
pub fn key(text: &str) -> String {
    style(text).bold().to_string()
}

/// Style text as a value
pub fn value(text: &str) -> String {
    style(text).cyan().to_string()
}

/// Style text for success/positive status
pub fn status_ok(text: &str) -> String {
    style(text).green().to_string()
}

/// Style text for pending/in-progress status
pub fn status_pending(text: &str) -> String {
    style(text).yellow().to_string()
}

/// Style text for error/failed status
pub fn status_error(text: &str) -> String {
    style(text).red().to_string()
}

/// Create a spinner for indeterminate progress
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .expect("valid template"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Create a progress bar for determinate progress
pub fn progress_bar(total: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg}\n    [{bar:40.cyan/dim}] {pos}/{len} ({percent}%)")
            .expect("valid template")
            .progress_chars("=>-"),
    );
    pb.set_message(msg.to_string());
    pb
}

/// Create a progress bar for byte transfers
pub fn transfer_bar(total: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg}\n    [{bar:40.cyan/dim}] {bytes}/{total_bytes} ({bytes_per_sec})")
            .expect("valid template")
            .progress_chars("=>-"),
    );
    pb.set_message(msg.to_string());
    pb
}

/// Finish a progress bar with a success message
pub fn finish_success(pb: &ProgressBar, msg: &str) {
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!("{} {{msg}}", style("[+]").green().bold()))
            .expect("valid template"),
    );
    pb.finish_with_message(msg.to_string());
}

/// Finish a progress bar with an error message
pub fn finish_error(pb: &ProgressBar, msg: &str) {
    pb.set_style(
        ProgressStyle::default_bar()
            .template(&format!("{} {{msg}}", style("[!]").red().bold()))
            .expect("valid template"),
    );
    pb.finish_with_message(msg.to_string());
}

/// Format bytes in human-readable form
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Print a header/section title
pub fn header(text: &str) {
    println!();
    println!("{}", style(text).bold().underlined());
}

/// Print a simple separator line
pub fn separator() {
    println!("{}", style("─".repeat(60)).dim());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn test_output_mode_default() {
        let mode = OutputMode::default();
        assert!(matches!(mode, OutputMode::Human));
    }
}
