//! JSON with comments, as Kilo, OpenCode and Gemini CLI write their configuration: `//`
//! and `/* */` comments and trailing commas allowed.

use std::path::Path;

use serde_json::Value;

/// A JSON-with-comments file. None where it is missing or no such JSON.
pub(crate) fn jsonc(path: &Path) -> Option<Value> {
    parse(&std::fs::read_to_string(path).ok()?).ok()
}

/// A JSON-with-comments text as JSON, a leading byte-order mark allowed.
pub(crate) fn parse(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(&plain(text.strip_prefix('\u{feff}').unwrap_or(text)))
}

/// JSON with comments as plain JSON: each comment and each comma before a closing bracket
/// dropped, strings kept whole.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    // Where the last comma stands in `out`, until a token other than white space follows.
    let mut comma: Option<usize> = None;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                comma = None;
                out.push(c);
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = '\0';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            ',' => {
                comma = Some(out.len());
                out.push(c);
            }
            '}' | ']' => {
                if let Some(at) = comma.take() {
                    out.replace_range(at..=at, " ");
                }
                out.push(c);
            }
            c if c.is_whitespace() => out.push(c),
            c => {
                comma = None;
                out.push(c);
            }
        }
    }
    out
}
