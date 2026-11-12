//! A systemd `EnvironmentFile`, read the way systemd reads it and not the way a shell would.
//!
//! The service's file holds connection strings and a password, which is where `$`, `&`, `;`, `#`
//! and quotes live, so sourcing it as shell mangles values or runs them.

/// The variables a file sets, and a warning for every line that was not understood.
pub struct Parsed {
    pub vars: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

pub fn parse(text: &str) -> Parsed {
    let mut vars = Vec::new();
    let mut warnings = Vec::new();

    // A trailing backslash joins the next line, the backslash becoming a space.
    let mut logical: Vec<String> = Vec::new();
    let mut carry: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let joined = match carry.take() {
            Some(prev) => format!("{prev} {line}"),
            None => line.to_string(),
        };
        match joined.strip_suffix('\\') {
            Some(rest) => carry = Some(rest.to_string()),
            None => logical.push(joined),
        }
    }
    logical.extend(carry);

    for line in logical {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("ignoring a line with no `=`: {line:?}"));
            continue;
        };
        vars.push((key.trim().to_string(), unquote(value.trim())));
    }
    Parsed { vars, warnings }
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == bytes[bytes.len() - 1] {
        let inner = &value[1..value.len() - 1];
        match bytes[0] {
            b'\'' => return inner.to_string(),
            b'"' => return unescape(inner),
            _ => {}
        }
    }
    value.to_string()
}

fn unescape(inner: &str) -> String {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(c @ ('\\' | '"' | '\'')) => out.push(c),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> String {
        let p = parse(line);
        assert_eq!(p.vars.len(), 1, "{line:?}: {:?}", p.vars);
        assert_eq!(p.vars[0].0, "KEY");
        p.vars[0].1.clone()
    }

    #[test]
    fn a_value_is_literal_with_no_expansion_splitting_or_separators() {
        assert_eq!(one("KEY=pa$word"), "pa$word");
        assert_eq!(one("KEY=two words"), "two words");
        assert_eq!(one(r#"KEY=it"s"#), r#"it"s"#);
        assert_eq!(one("KEY=x;rm -f canary"), "x;rm -f canary");
        assert_eq!(one("KEY=`id`"), "`id`");
    }

    #[test]
    fn a_url_keeps_its_ampersands_question_marks_and_hashes() {
        assert_eq!(
            one("KEY=postgresql://u:p@h:5433/db?sslmode=require&x=1"),
            "postgresql://u:p@h:5433/db?sslmode=require&x=1"
        );
        assert_eq!(one("KEY=ab#cd"), "ab#cd");
    }

    #[test]
    fn comments_blank_lines_and_crlf_are_handled() {
        let p = parse("; a comment\r\n# another\r\n\r\nKEY=v\r\n");
        assert_eq!(p.vars, vec![("KEY".to_string(), "v".to_string())]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn one_pair_of_matching_quotes_is_removed() {
        assert_eq!(one("KEY='a b $c'"), "a b $c");
        assert_eq!(one(r#"KEY="a b""#), "a b");
        assert_eq!(one(r#"KEY="say \"hi\"\\ \n""#), "say \"hi\"\\ \n");
        assert_eq!(one(r"KEY='no \n escapes'"), r"no \n escapes");
        assert_eq!(one(r#"KEY="mismatched'"#), r#""mismatched'"#);
    }

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        assert_eq!(one("KEY=a\\\nb"), "a b");
    }

    #[test]
    fn a_line_without_an_equals_sign_is_ignored_with_a_warning() {
        let p = parse("nonsense\nKEY=v\n");
        assert_eq!(p.vars.len(), 1);
        assert_eq!(p.warnings.len(), 1);
    }
}
