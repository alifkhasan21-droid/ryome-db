//! Helper output kecil: warna ANSI (otomatis mati kalau bukan TTY / NO_COLOR).

use std::io::IsTerminal;
use std::sync::OnceLock;

fn color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
    })
}

fn paint(code: &str, s: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn ok(msg: impl AsRef<str>) {
    eprintln!("{} {}", green("✓"), msg.as_ref());
}
pub fn warn(msg: impl AsRef<str>) {
    eprintln!("{} {}", yellow("!"), msg.as_ref());
}
pub fn fail(msg: impl AsRef<str>) {
    eprintln!("{} {}", red("✗"), msg.as_ref());
}
pub fn info(msg: impl AsRef<str>) {
    eprintln!("{} {}", dim("·"), msg.as_ref());
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
