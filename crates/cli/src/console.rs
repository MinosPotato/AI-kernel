//! Reading lines from the terminal.
//!
//! One reader for the whole process. The conversation prompt and the approval prompt both
//! read from standard input, never at the same time — an approval is only ever asked while
//! a run is in flight, and a run is only ever started after a line has been read — so
//! sharing one reader is what keeps them from competing for the same bytes.
//!
//! Generic over the reader so a test can drive a whole session from a byte slice.

use std::io::Write as _;

use aik_core::{Error, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines, Stdin, stdin};

/// A line-oriented view of an input stream, with prompts written to standard output.
#[derive(Debug)]
pub struct Console<R> {
    lines: Lines<R>,
}

impl Console<BufReader<Stdin>> {
    /// A console reading the process's standard input.
    pub fn stdio() -> Self {
        Self::new(BufReader::new(stdin()))
    }
}

impl<R: AsyncBufRead + Unpin + Send> Console<R> {
    /// A console reading from `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            lines: reader.lines(),
        }
    }

    /// Writes `prompt` and reads one line, or `None` at end of input.
    ///
    /// End of input is not an error: a piped session that runs out of input has ended, and
    /// for an approval it means nobody is there to answer — which is a refusal.
    pub async fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        print!("{prompt}");
        std::io::stdout()
            .flush()
            .map_err(|error| Error::wrap("writing to standard output", error))?;

        self.lines
            .next_line()
            .await
            .map_err(|error| Error::wrap("reading from standard input", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(input: &'static str) -> Console<&'static [u8]> {
        Console::new(input.as_bytes())
    }

    #[tokio::test]
    async fn lines_are_returned_in_order_without_their_terminator() {
        let mut console = console("first\nsecond\n");
        assert_eq!(console.ask("> ").await.unwrap().as_deref(), Some("first"));
        assert_eq!(console.ask("> ").await.unwrap().as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn exhausted_input_reads_as_end_of_input_rather_than_an_error() {
        let mut console = console("only\n");
        assert!(console.ask("> ").await.unwrap().is_some());
        assert_eq!(console.ask("> ").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_final_line_without_a_newline_is_still_read() {
        let mut console = console("no trailing newline");
        assert_eq!(
            console.ask("> ").await.unwrap().as_deref(),
            Some("no trailing newline"),
        );
    }
}
