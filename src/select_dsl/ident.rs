use anyhow::{Context, Result, bail};

/// Парсит список идентификаторов в виде `(a, b, "C D")`.
pub(super) fn parse_parenthesized_identifier_list(raw: &str) -> Result<Vec<String>> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with('(') {
        bail!("EXCEPT clause must be followed by '(col1, col2, ...)'");
    }

    let close_index = find_matching_paren(trimmed)
        .with_context(|| format!("unterminated EXCEPT list in '{trimmed}'"))?;
    let after = trimmed[close_index + 1..].trim();
    if !after.is_empty() {
        bail!("unexpected trailing content after EXCEPT list: '{after}'");
    }

    parse_identifier_list(&trimmed[1..close_index])
}

fn find_matching_paren(input: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut depth = 0_usize;
    let mut i = 0;
    let mut in_double_quote = false;

    while i < bytes.len() {
        let ch = bytes[i];

        if in_double_quote {
            if ch == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double_quote = false;
            }
            i += 1;
            continue;
        }

        match ch {
            b'"' => {
                in_double_quote = true;
            }
            b'(' => {
                depth += 1;
            }
            b')' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }

        i += 1;
    }

    None
}

pub(super) fn parse_source_relation(input: &str) -> Result<(String, String, &str)> {
    let (schema, consumed_schema) = parse_relation_identifier_at_start(input)
        .with_context(|| format!("invalid source schema in '{input}'"))?;
    let after_schema = input[consumed_schema..].trim_start();
    let Some(after_dot) = after_schema.strip_prefix('.') else {
        bail!("source relation must be fully qualified as schema.object");
    };

    let (name, consumed_name) = parse_relation_identifier_at_start(after_dot)
        .with_context(|| format!("invalid source object name in '{input}'"))?;
    let tail = after_dot[consumed_name..].trim_start();

    Ok((schema, name, tail))
}

/// Парсит CSV-список идентификаторов.
pub(super) fn parse_identifier_list(input: &str) -> Result<Vec<String>> {
    let mut rest = input.trim();
    if rest.is_empty() {
        bail!("identifier list must not be empty");
    }

    let mut identifiers = Vec::new();
    while !rest.is_empty() {
        let (identifier, consumed) = parse_identifier_at_start(rest)
            .with_context(|| format!("invalid identifier in '{input}'"))?;
        identifiers.push(identifier);
        rest = rest[consumed..].trim_start();

        if rest.is_empty() {
            break;
        }

        let Some(after_comma) = rest.strip_prefix(',') else {
            bail!("expected ',' between identifiers in '{input}'");
        };
        rest = after_comma.trim_start();
        if rest.is_empty() {
            bail!("identifier list must not end with ','");
        }
    }

    Ok(identifiers)
}

fn parse_identifier_at_start(input: &str) -> Result<(String, usize)> {
    let trimmed = input.trim_start();
    let leading_ws = input.len() - trimmed.len();
    let first = trimmed
        .chars()
        .next()
        .with_context(|| "identifier is missing".to_owned())?;

    if first == '"' {
        let (identifier, consumed) = parse_quoted_identifier(trimmed)?;
        return Ok((identifier, leading_ws + consumed));
    }

    parse_unquoted_identifier(trimmed)
        .map(|(identifier, consumed)| (identifier, leading_ws + consumed))
}

fn parse_relation_identifier_at_start(input: &str) -> Result<(String, usize)> {
    let trimmed = input.trim_start();
    let leading_ws = input.len() - trimmed.len();
    let first = trimmed
        .chars()
        .next()
        .with_context(|| "identifier is missing".to_owned())?;

    if first == '"' {
        let (identifier, consumed) = parse_quoted_identifier(trimmed)?;
        return Ok((identifier, leading_ws + consumed));
    }

    parse_unquoted_relation_identifier(trimmed)
        .map(|(identifier, consumed)| (identifier, leading_ws + consumed))
}

fn parse_quoted_identifier(input: &str) -> Result<(String, usize)> {
    let mut chars = input.char_indices();
    let Some((_, first)) = chars.next() else {
        bail!("quoted identifier is empty");
    };
    if first != '"' {
        bail!("quoted identifier must start with '\"'");
    }

    let mut value = String::new();
    while let Some((index, ch)) = chars.next() {
        if ch == '"' {
            if let Some((_, next)) = chars.clone().next()
                && next == '"'
            {
                // PostgreSQL escaping внутри quoted identifier: "" => "
                value.push('"');
                chars.next();
                continue;
            }

            if value.is_empty() {
                bail!("quoted identifier must not be empty");
            }
            return Ok((value, index + 1));
        }

        value.push(ch);
    }

    bail!("unterminated quoted identifier")
}

fn parse_unquoted_identifier(input: &str) -> Result<(String, usize)> {
    parse_unquoted_identifier_with_start(input, is_unquoted_identifier_start, "[A-Za-z_]")
}

fn parse_unquoted_relation_identifier(input: &str) -> Result<(String, usize)> {
    parse_unquoted_identifier_with_start(
        input,
        is_unquoted_relation_identifier_start,
        "[A-Za-z0-9_]",
    )
}

fn parse_unquoted_identifier_with_start(
    input: &str,
    is_start: fn(char) -> bool,
    start_hint: &str,
) -> Result<(String, usize)> {
    let mut consumed = 0_usize;
    for (index, ch) in input.char_indices() {
        if index == 0 {
            if !is_start(ch) {
                bail!(
                    "invalid identifier start '{ch}': expected {start_hint} or quoted identifier"
                );
            }
            consumed = index + ch.len_utf8();
            continue;
        }

        if !is_unquoted_identifier_char(ch) {
            break;
        }
        consumed = index + ch.len_utf8();
    }

    if consumed == 0 {
        bail!("identifier is missing");
    }

    Ok((input[..consumed].to_ascii_lowercase(), consumed))
}

fn is_unquoted_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_unquoted_relation_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_unquoted_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}
