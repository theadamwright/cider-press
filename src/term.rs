//! Terminal output: colour handling, status glyphs, and the banner.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Should output be coloured?
///
/// Only when stdout is a terminal and NO_COLOR is unset, so piping into a file
/// or `grep` gives clean text. Computed once.
fn colour() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

macro_rules! style {
    ($name:ident, $code:expr) => {
        pub fn $name(s: &str) -> String {
            if colour() {
                format!("\x1b[{}m{s}\x1b[0m", $code)
            } else {
                s.to_string()
            }
        }
    };
}

style!(bold, "1");
style!(dim, "2");
style!(red, "31");
style!(green, "32");
style!(yellow, "33");
style!(amber, "38;5;208");
// Combined, because nesting two styles leaves a reset in the middle that
// cancels the outer one.
style!(bold_amber, "1;38;5;208");

pub fn info(msg: &str) {
    println!("{} {msg}", amber("::"));
}
pub fn ok(msg: &str) {
    println!("  {} {msg}", green("✔"));
}
pub fn warn(msg: &str) {
    println!("  {} {msg}", yellow("!"));
}
pub fn bad(msg: &str) {
    println!("  {} {msg}", red("✘"));
}
/// A check that could not run because an earlier one failed. Not a pass, not a
/// failure — just not reached.
pub fn skip(msg: &str) {
    println!("  {} {}", dim("–"), dim(msg));
}
pub fn note(msg: &str) {
    println!("      {}", dim(msg));
}

pub const ART: &str = r#"        \ | /
      .--'--.            .-------.
     /        \          |~~~~~~~|
    |    ()    |         |~~~~~~~|
     \        /          |~~~~~~~|
      '-.__.-'           '._____.'
"#;

/// The apple-and-glass banner, printed by the commands that take a while.
pub fn banner() {
    print!("{ART}");
    println!(
        "\n   {}  {}\n",
        bold_amber("cider-press"),
        dim("PGD, pressed on Apple container")
    );
}
