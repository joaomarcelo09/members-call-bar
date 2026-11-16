//! `discord-voice focus`: runs `focus_command` (Waybar `on-click`) to bring
//! the Discord window to the front. No shell is involved: the command is
//! split with simple shell-like quoting rules and executed directly.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{bail, Result};

/// Splits a command line into words: whitespace separates words, `'...'`
/// is literal, `"..."` allows `\"`, `\\`, `\$` and `` \` `` escapes, and a
/// backslash outside quotes escapes the next character. No expansion.
pub fn split_command(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => bail!("unterminated single quote"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => bail!("unterminated double quote"),
                        },
                        Some(c) => word.push(c),
                        None => bail!("unterminated double quote"),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => bail!("trailing backslash"),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Starts `argv` detached (own process group, no stdio) and returns at
/// once without waiting. Errors are ignored: a click must never fail.
pub fn spawn_detached(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        split_command(s).unwrap()
    }

    #[test]
    fn splits_words_and_quotes() {
        assert_eq!(
            split("hyprctl dispatch focuswindow class:discord"),
            ["hyprctl", "dispatch", "focuswindow", "class:discord"]
        );
        assert_eq!(split("  a\t b  "), ["a", "b"]);
        assert_eq!(split("a 'b c' \"d e\""), ["a", "b c", "d e"]);
        assert_eq!(split("a'b'\"c\"d"), ["abcd"]);
        assert_eq!(split(r#""a \"q\" \$x \n""#), [r#"a "q" $x \n"#]);
        assert_eq!(split(r"a\ b \'c"), ["a b", "'c"]);
        assert_eq!(split("'' x"), ["", "x"]);
        assert_eq!(split("echo $HOME; rm"), ["echo", "$HOME;", "rm"]);
        assert!(split("").is_empty());
    }

    #[test]
    fn rejects_unbalanced_quotes() {
        assert!(split_command("a 'b").is_err());
        assert!(split_command("a \"b").is_err());
        assert!(split_command("a \\").is_err());
    }

    #[test]
    fn spawn_ignores_missing_programs() {
        spawn_detached(&[]);
        spawn_detached(&["/nonexistent/discord-voice-test-program".to_owned()]);
        spawn_detached(&["true".to_owned()]);
    }
}
