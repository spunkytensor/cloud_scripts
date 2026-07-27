// SPDX-FileCopyrightText: 2026 Matt Curfman
// SPDX-License-Identifier: Apache-2.0

use std::{env, io::IsTerminal};

const GREEN: &str = "32";
const BLUE: &str = "34";
const RED: &str = "31";
const DIM: &str = "90";

/// Returns whether stderr is an interactive terminal that supports status colors.
fn colors_enabled() -> bool {
    std::io::stderr().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").is_ok_and(|term| term != "dumb")
}

/// Writes one consistently indented status line, applying ANSI color only on a terminal.
fn status(symbol: &str, label: &str, message: &str, color: &str) {
    let prefix = format!("{symbol} {label:<14}");
    if colors_enabled() {
        eprintln!("  \x1b[1;{color}m{prefix}\x1b[0m {message}");
    } else {
        eprintln!("  {prefix} {message}");
    }
}

/// Reports an in-progress operation.
pub fn pending(label: &str, message: impl AsRef<str>) {
    status("→", label, message.as_ref(), DIM);
}

/// Reports a successfully completed operation.
pub fn success(label: &str, message: impl AsRef<str>) {
    status("✓", label, message.as_ref(), GREEN);
}

/// Reports an operator action or informational next step.
pub fn action(label: &str, message: impl AsRef<str>) {
    status("◆", label, message.as_ref(), BLUE);
}

/// Reports an error message to stderr.
pub fn error(message: impl AsRef<str>) {
    status("✗", "Error", message.as_ref(), RED);
}

/// Starts a console section with a prominent label and subject.
pub fn heading(label: &str, message: impl AsRef<str>) {
    let heading = format!("{label:<16}");
    if colors_enabled() {
        eprintln!("\n\x1b[1m{heading}\x1b[0m {}\n", message.as_ref());
    } else {
        eprintln!("\n{heading} {}\n", message.as_ref());
    }
}
