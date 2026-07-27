use std::{env, io::IsTerminal};

const GREEN: &str = "32";
const BLUE: &str = "34";
const RED: &str = "31";
const DIM: &str = "90";

fn colors_enabled() -> bool {
    std::io::stderr().is_terminal()
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").is_ok_and(|term| term != "dumb")
}

fn status(symbol: &str, label: &str, message: &str, color: &str) {
    let prefix = format!("{symbol} {label:<14}");
    if colors_enabled() {
        eprintln!("  \x1b[1;{color}m{prefix}\x1b[0m {message}");
    } else {
        eprintln!("  {prefix} {message}");
    }
}

pub fn pending(label: &str, message: impl AsRef<str>) {
    status("→", label, message.as_ref(), DIM);
}

pub fn success(label: &str, message: impl AsRef<str>) {
    status("✓", label, message.as_ref(), GREEN);
}

pub fn action(label: &str, message: impl AsRef<str>) {
    status("◆", label, message.as_ref(), BLUE);
}

pub fn error(message: impl AsRef<str>) {
    status("✗", "Error", message.as_ref(), RED);
}

pub fn heading(label: &str, message: impl AsRef<str>) {
    let heading = format!("{label:<16}");
    if colors_enabled() {
        eprintln!("\n\x1b[1m{heading}\x1b[0m {}\n", message.as_ref());
    } else {
        eprintln!("\n{heading} {}\n", message.as_ref());
    }
}
