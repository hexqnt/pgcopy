use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;

static RE_PREFIX_WHERE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)^where\b(.*)$").expect("valid regex"));
static RE_PREFIX_ORDER_BY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)^order\s+by\b(.*)$").expect("valid regex"));
static RE_PREFIX_LIMIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)^limit\b(.*)$").expect("valid regex"));

#[derive(Debug, Default)]
pub(super) struct ParsedClauses {
    pub(super) where_clause: Option<String>,
    pub(super) order_by_clause: Option<String>,
    pub(super) limit_clause: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailClause {
    OrderBy,
    Limit,
}

/// Снимает ведущий keyword без учёта регистра.
pub(super) fn strip_leading_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = input.trim_start();
    if !has_keyword_at(trimmed, 0, keyword) {
        return None;
    }
    Some(trimmed[keyword.len()..].trim_start())
}

/// Делит строку по keyword, игнорируя вхождения внутри кавычек.
pub(super) fn split_on_keyword<'a>(input: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let index = find_keyword_outside_quotes(input, keyword, false)?;
    let before = input[..index].trim_end();
    let after = input[index + keyword.len()..].trim_start();
    Some((before, after))
}

/// Парсит поддерживаемый хвост DSL: `WHERE ... ORDER BY ... LIMIT ...`.
pub(super) fn parse_optional_clauses(tail: &str) -> Result<ParsedClauses> {
    let mut rest = tail.trim();
    if rest.is_empty() {
        return Ok(ParsedClauses::default());
    }

    if rest.contains(';') {
        bail!("select DSL clauses must not contain ';'");
    }

    let mut parsed = ParsedClauses::default();

    if let Some(after_where) = strip_prefix_clause(rest, &RE_PREFIX_WHERE) {
        let (where_clause, remaining) = split_clause(before_next_clause(
            after_where,
            &[TailClause::OrderBy, TailClause::Limit],
        ));
        parsed.where_clause = Some(parse_non_empty_clause("WHERE", where_clause)?);
        rest = remaining;
    }

    if let Some(after_order_by) = strip_prefix_clause(rest, &RE_PREFIX_ORDER_BY) {
        let (order_by_clause, remaining) =
            split_clause(before_next_clause(after_order_by, &[TailClause::Limit]));
        parsed.order_by_clause = Some(parse_non_empty_clause("ORDER BY", order_by_clause)?);
        rest = remaining;
    }

    if let Some(after_limit) = strip_prefix_clause(rest, &RE_PREFIX_LIMIT) {
        let limit_raw = after_limit.trim();
        if limit_raw.is_empty() {
            bail!("LIMIT value must not be empty");
        }

        if limit_raw.split_whitespace().count() > 1 {
            bail!("unsupported trailing clause after LIMIT: '{limit_raw}'");
        }

        let limit = limit_raw.parse::<u64>().with_context(|| {
            format!("invalid LIMIT value '{limit_raw}': expected non-negative integer")
        })?;
        parsed.limit_clause = Some(limit);
        rest = "";
    }

    if !rest.is_empty() {
        bail!(
            "unsupported trailing clause in select DSL: '{rest}'. Allowed tail clauses: WHERE, ORDER BY, LIMIT (in this order)"
        );
    }

    Ok(parsed)
}

fn strip_prefix_clause<'a>(input: &'a str, prefix: &Regex) -> Option<&'a str> {
    prefix
        .captures(input)
        .and_then(|captures| captures.get(1).map(|m| m.as_str()))
}

fn parse_non_empty_clause(name: &str, raw: &str) -> Result<String> {
    let clause = raw.trim();
    if clause.is_empty() {
        bail!("{name} clause must not be empty");
    }
    Ok(clause.to_owned())
}

fn split_clause<'a>((clause, remaining): (&'a str, &'a str)) -> (&'a str, &'a str) {
    (clause.trim_end(), remaining.trim_start())
}

fn before_next_clause<'a>(input: &'a str, clauses: &[TailClause]) -> (&'a str, &'a str) {
    if let Some(index) = find_next_clause_start(input, clauses) {
        let (before, after) = input.split_at(index);
        return (before, after);
    }

    (input, "")
}

fn find_next_clause_start(input: &str, clauses: &[TailClause]) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut previous_word: Option<(String, usize)> = None;

    while i < bytes.len() {
        let ch = bytes[i];

        if in_single_quote {
            if ch == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            }
            i += 1;
            continue;
        }

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

        if ch == b'\'' {
            in_single_quote = true;
            i += 1;
            continue;
        }

        if ch == b'"' {
            in_double_quote = true;
            i += 1;
            continue;
        }

        if is_word_char(ch) {
            let start = i;
            while i < bytes.len() && is_word_char(bytes[i]) {
                i += 1;
            }
            let word = input[start..i].to_ascii_lowercase();

            if clauses.contains(&TailClause::Limit) && word == "limit" {
                return Some(start);
            }

            if clauses.contains(&TailClause::OrderBy)
                && let Some((prev_word, prev_start)) = &previous_word
                && prev_word == "order"
                && word == "by"
            {
                return Some(*prev_start);
            }

            previous_word = Some((word, start));
            continue;
        }

        if !ch.is_ascii_whitespace() {
            previous_word = None;
        }

        i += 1;
    }

    None
}

fn find_keyword_outside_quotes(input: &str, keyword: &str, allow_parens: bool) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut paren_depth = 0_usize;

    while i < bytes.len() {
        let ch = bytes[i];

        if in_single_quote {
            if ch == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            }
            i += 1;
            continue;
        }

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
            b'\'' => {
                in_single_quote = true;
                i += 1;
                continue;
            }
            b'"' => {
                in_double_quote = true;
                i += 1;
                continue;
            }
            b'(' => {
                paren_depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            _ => {}
        }

        // Ключевые слова внутри скобок игнорируем, чтобы не ломать выражения.
        if (allow_parens || paren_depth == 0) && has_keyword_at(input, i, keyword) {
            return Some(i);
        }

        i += 1;
    }

    None
}

fn has_keyword_at(input: &str, index: usize, keyword: &str) -> bool {
    if index + keyword.len() > input.len() {
        return false;
    }

    if !input[index..index + keyword.len()].eq_ignore_ascii_case(keyword) {
        return false;
    }

    let before = if index == 0 {
        None
    } else {
        input.as_bytes()[..index].last().copied()
    };
    let after = if index + keyword.len() >= input.len() {
        None
    } else {
        input.as_bytes()[index + keyword.len()..].first().copied()
    };

    if before.is_some_and(is_word_char) {
        return false;
    }
    if after.is_some_and(is_word_char) {
        return false;
    }

    true
}

const fn is_word_char(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || ch == b'_'
}
