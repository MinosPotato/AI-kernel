//! Turning a fetched HTML document into the text a model can actually use.
//!
//! This is not a parser and does not try to be one. It has one job — produce readable text
//! from markup, cheaply, without pulling a browser engine's worth of dependency into a
//! security-sensitive crate — and it is written so that its failure mode is *worse text*,
//! never a wrong decision: nothing here is consulted by any check, no output of it reaches
//! anything but the model, and every branch it cannot make sense of degrades to "treat it
//! as text".
//!
//! What it does do is drop the parts of a document that are not prose and would otherwise
//! dominate it: the contents of `script`, `style`, `template`, `noscript` and `svg`
//! elements, comments, doctypes and processing instructions, and attribute values. That is
//! not a security measure — a model reading a fetched page must in every case be assumed to
//! be reading text somebody else wrote — it is what keeps a page's worth of minified
//! JavaScript from being charged to the conversation as input tokens.

/// Elements whose *contents* are dropped along with their tags.
const OPAQUE: [&str; 5] = ["script", "style", "template", "noscript", "svg"];

/// Elements that end a line of text.
const BLOCK: [&str; 22] = [
    "p",
    "div",
    "br",
    "hr",
    "li",
    "ul",
    "ol",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "aside",
    "blockquote",
    "pre",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
];

/// Text extracted from a document, and whether it was cut short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Extracted {
    /// The text.
    pub(crate) text: String,
    /// Whether the limit stopped it early.
    pub(crate) truncated: bool,
}

/// Extracts readable text from HTML, stopping after `limit` bytes of output.
pub(crate) fn html_to_text(input: &str, limit: usize) -> Extracted {
    let mut sink = Sink::new(limit);
    let mut rest = input;

    while !rest.is_empty() && !sink.full() {
        let Some(open) = rest.find('<') else {
            sink.push_text(rest);
            break;
        };
        sink.push_text(&rest[..open]);
        rest = &rest[open..];

        if let Some(after) = rest.strip_prefix("<!--") {
            rest = skip_to(after, "-->");
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            rest = skip_past_tag(&rest[1..]);
            continue;
        }

        let after_bracket = &rest[1..];
        let closing = after_bracket.starts_with('/');
        let name_start = if closing { 1 } else { 0 };
        let name = tag_name(&after_bracket[name_start..]);
        if name.is_empty() {
            // A bare `<` in prose. Emit it and carry on rather than swallowing the rest.
            sink.push_text("<");
            rest = after_bracket;
            continue;
        }

        let after_tag = skip_past_tag(&after_bracket[name_start + name.len()..]);
        if !closing && OPAQUE.contains(&name.as_str()) {
            rest = skip_element(after_tag, &name);
            continue;
        }
        if BLOCK.contains(&name.as_str()) || name == "h6" {
            sink.push_newline();
        }
        rest = after_tag;
    }

    sink.finish()
}

/// Everything after the first occurrence of `marker`, or nothing.
fn skip_to<'a>(input: &'a str, marker: &str) -> &'a str {
    match input.find(marker) {
        Some(at) => &input[at + marker.len()..],
        None => "",
    }
}

/// Everything after this tag's `>`, ignoring one inside a quoted attribute value.
fn skip_past_tag(input: &str) -> &str {
    let mut quote: Option<char> = None;
    for (index, character) in input.char_indices() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '>') => return &input[index + character.len_utf8()..],
            (None, _) => {}
        }
    }
    ""
}

/// Everything after this element's closing tag.
fn skip_element<'a>(input: &'a str, name: &str) -> &'a str {
    let needle = format!("</{name}");
    let lowered = input.to_ascii_lowercase();
    match lowered.find(&needle) {
        Some(at) => skip_past_tag(&input[at + needle.len()..]),
        None => "",
    }
}

/// The tag name at the start of `input`, lowercased.
fn tag_name(input: &str) -> String {
    input
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Accumulates output, collapsing whitespace and honouring the limit.
struct Sink {
    out: String,
    limit: usize,
    truncated: bool,
}

impl Sink {
    fn new(limit: usize) -> Self {
        Self {
            out: String::new(),
            limit,
            truncated: false,
        }
    }

    fn full(&self) -> bool {
        self.out.len() >= self.limit
    }

    fn push_newline(&mut self) {
        if self.out.is_empty() || self.full() {
            return;
        }
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn push_text(&mut self, raw: &str) {
        for character in decode(raw).chars() {
            if self.full() {
                self.truncated = true;
                return;
            }
            if character.is_whitespace() {
                if !self.out.is_empty() && !self.out.ends_with(char::is_whitespace) {
                    self.out.push(' ');
                }
            } else {
                self.out.push(character);
            }
        }
    }

    fn finish(mut self) -> Extracted {
        if self.out.len() >= self.limit {
            self.truncated = true;
        }
        Extracted {
            text: self.out.trim().to_owned(),
            truncated: self.truncated,
        }
    }
}

/// Replaces the character references a document is likely to contain.
fn decode(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        match after.find(';') {
            // A `&` that is not part of a reference — or one so long it cannot be.
            Some(end) if end <= 10 => {
                match named(&after[..end]) {
                    Some(replacement) => out.push_str(replacement),
                    None => match numeric(&after[..end]) {
                        Some(character) => out.push(character),
                        None => {
                            out.push('&');
                            out.push_str(&after[..end]);
                            out.push(';');
                        }
                    },
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn named(body: &str) -> Option<&'static str> {
    Some(match body {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        "nbsp" => " ",
        "hellip" => "...",
        "mdash" | "ndash" => "-",
        "rsquo" | "lsquo" => "'",
        "rdquo" | "ldquo" => "\"",
        _ => return None,
    })
}

fn numeric(body: &str) -> Option<char> {
    let digits = body.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> String {
        html_to_text(input, 64 * 1024).text
    }

    #[test]
    fn tags_are_dropped_and_their_text_is_kept() {
        assert_eq!(text("<p>hello <b>there</b></p>"), "hello there");
    }

    #[test]
    fn block_elements_become_line_breaks() {
        // Both the close and the open break, so paragraphs are separated by a blank line.
        assert_eq!(text("<p>one</p><p>two</p>"), "one\n\ntwo");
        assert_eq!(text("a<br>b"), "a\nb");
    }

    #[test]
    fn script_and_style_contents_never_reach_the_output() {
        let html = "<style>body{color:red}</style><p>visible</p>\
                    <script>var secret = 1;</script>";
        assert_eq!(text(html), "visible");
    }

    #[test]
    fn a_script_tag_with_attributes_is_still_skipped_whole() {
        let html = r#"<script type="text/javascript" src="a>b">payload</script>ok"#;
        assert_eq!(text(html), "ok");
    }

    #[test]
    fn an_unclosed_opaque_element_swallows_the_rest_rather_than_leaking_it() {
        assert_eq!(text("before<script>never ends"), "before");
    }

    #[test]
    fn comments_doctypes_and_attributes_are_dropped() {
        let html = "<!doctype html><!-- <p>commented</p> --><a href=\"http://x/\">link</a>";
        assert_eq!(text(html), "link");
    }

    #[test]
    fn a_greater_than_inside_an_attribute_does_not_end_the_tag_early() {
        assert_eq!(text(r#"<div title="a > b">text</div>"#), "text");
    }

    #[test]
    fn character_references_are_decoded() {
        assert_eq!(
            text("<p>a &amp; b &lt;c&gt; &#65; &#x42;</p>"),
            "a & b <c> A B"
        );
    }

    #[test]
    fn something_that_looks_like_a_reference_but_is_not_is_left_alone() {
        assert_eq!(text("<p>a &notareference; b</p>"), "a &notareference; b");
        assert_eq!(text("<p>tom & jerry</p>"), "tom & jerry");
    }

    #[test]
    fn runs_of_whitespace_collapse() {
        assert_eq!(text("<p>a   \n\t  b</p>"), "a b");
    }

    #[test]
    fn a_bare_angle_bracket_in_prose_is_kept_rather_than_swallowing_the_document() {
        assert_eq!(text("<p>3 < 4 is true</p>"), "3 < 4 is true");
    }

    #[test]
    fn output_stops_at_the_limit_and_says_so() {
        let extracted = html_to_text("<p>abcdefghijklmnop</p>", 8);
        assert!(extracted.truncated);
        assert!(extracted.text.len() <= 8);
    }

    #[test]
    fn a_document_under_the_limit_is_not_reported_as_truncated() {
        let extracted = html_to_text("<p>short</p>", 64);
        assert!(!extracted.truncated);
        assert_eq!(extracted.text, "short");
    }
}
