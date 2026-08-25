//! Rendering an argument vector as one unambiguous line.
//!
//! This exists for exactly one reason: a policy rule and a human approving a call both need to
//! see *what will run*, and both are matching or reading a single string. Turning a vector
//! into a string is where that goes wrong — `["git", "commit", "-m", "fix a; drop b"]`
//! rendered by joining on spaces is indistinguishable from four separate arguments, so a rule
//! written to allow `git commit -m fix` would match a command it was never shown.
//!
//! So the rendering is *injective*: distinct argument vectors always produce distinct lines.
//! Any argument that is not made purely of characters that cannot change how a line parses is
//! wrapped in single quotes, with embedded quotes escaped the way POSIX shells require. That
//! is the same rule `shlex.quote` applies, chosen because it is the form a reader already
//! knows how to interpret rather than an encoding invented here.
//!
//! # What it is not
//!
//! It is not a command to run. Nothing in this crate ever parses one of these lines back, or
//! passes one to a shell; the argument vector is carried to `execve` unchanged. This string
//! is a *name* for a call — the thing a policy matches and a person reads — and treating it as
//! anything else would reintroduce the shell this crate deliberately does not have.

/// The characters that need no quoting, because none of them can change how a line parses.
///
/// Deliberately conservative: `~`, `*`, `?`, `[`, `#` and `!` are all safe inside single
/// quotes and all meaningful outside them, so quoting them costs a pair of quotes and buys a
/// reader who does not have to know which shell they are imagining.
fn is_bare(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b',' | b'+' | b'@' | b'%'
        )
}

/// Renders one argument, quoted if it needs to be.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && argument.bytes().all(is_bare) {
        return argument.to_owned();
    }
    // The POSIX idiom: a single-quoted string cannot contain a single quote, so each one is
    // closed, escaped outside the quotes, and reopened.
    format!("'{}'", argument.replace('\'', r"'\''"))
}

/// Renders a program and its arguments as one line.
pub(crate) fn render(program: &str, arguments: &[String]) -> String {
    let mut line = quote(program);
    for argument in arguments {
        line.push(' ');
        line.push_str(&quote(argument));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(program: &str, arguments: &[&str]) -> String {
        let arguments: Vec<String> = arguments.iter().map(|a| (*a).to_owned()).collect();
        render(program, &arguments)
    }

    #[test]
    fn ordinary_arguments_are_left_alone() {
        assert_eq!(
            line("git", &["log", "--oneline", "-n", "5", "src/lib.rs"]),
            "git log --oneline -n 5 src/lib.rs"
        );
    }

    #[test]
    fn an_argument_containing_a_space_cannot_look_like_two() {
        let one = line("git", &["commit", "-m", "fix a"]);
        let two = line("git", &["commit", "-m", "fix", "a"]);

        assert_eq!(one, "git commit -m 'fix a'");
        assert_ne!(one, two);
    }

    #[test]
    fn an_empty_argument_is_still_visible() {
        assert_eq!(line("echo", &[""]), "echo ''");
    }

    #[test]
    fn a_quote_cannot_close_the_quoting_around_it() {
        assert_eq!(line("echo", &["it's"]), r"echo 'it'\''s'");
        assert_eq!(line("echo", &["' 'x"]), r"echo ''\'' '\''x'");
    }

    #[test]
    fn shell_metacharacters_are_quoted_rather_than_hidden() {
        assert_eq!(
            line("echo", &["a; rm -rf /", "$HOME", "`id`", "*", "|", "&&"]),
            "echo 'a; rm -rf /' '$HOME' '`id`' '*' '|' '&&'"
        );
    }

    #[test]
    fn distinct_vectors_render_distinctly() {
        let vectors: Vec<Vec<&str>> = vec![
            vec!["a b"],
            vec!["a", "b"],
            vec!["a'b"],
            vec!["a", "", "b"],
            vec!["'a b'"],
        ];
        let rendered: Vec<String> = vectors.iter().map(|v| line("p", v)).collect();

        let unique: std::collections::BTreeSet<&String> = rendered.iter().collect();
        assert_eq!(unique.len(), rendered.len(), "{rendered:?}");
    }
}
