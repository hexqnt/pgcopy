use std::iter::{Enumerate, Peekable};
use std::str::Bytes;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailClause {
    OrderBy,
    Limit,
}

#[derive(Debug, Default)]
pub(super) struct ParsedClauses {
    pub(super) where_clause: Option<String>,
    pub(super) order_by_clause: Option<String>,
    pub(super) limit_clause: Option<u64>,
}

#[derive(Clone, Copy)]
struct WordToken<'a> {
    word: &'a str,
    start: usize,
}

struct TopLevelWordIter<'a> {
    input: &'a str,
    bytes: Peekable<Enumerate<Bytes<'a>>>,
    paren_depth: usize,
    quote: Option<u8>,
}

impl<'a> TopLevelWordIter<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.bytes().enumerate().peekable(),
            paren_depth: 0,
            quote: None,
        }
    }
}

impl<'a> Iterator for TopLevelWordIter<'a> {
    type Item = WordToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((index, ch)) = self.bytes.next() {
            if let Some(quote) = self.quote {
                if ch == quote {
                    if self.bytes.peek().is_some_and(|(_, next)| *next == quote) {
                        self.bytes.next();
                        continue;
                    }
                    self.quote = None;
                }
                continue;
            }

            match ch {
                b'\'' | b'"' => {
                    self.quote = Some(ch);
                    continue;
                }
                b'(' => {
                    self.paren_depth += 1;
                    continue;
                }
                b')' => {
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    continue;
                }
                _ => {}
            }

            if self.paren_depth == 0 && is_word_char(ch) {
                let start = index;
                let mut end = index + 1;
                while let Some(&(next_index, next_ch)) = self.bytes.peek() {
                    if !is_word_char(next_ch) {
                        break;
                    }
                    end = next_index + 1;
                    self.bytes.next();
                }
                return Some(WordToken {
                    word: &self.input[start..end],
                    start,
                });
            }
        }

        None
    }
}
/// Снимает ведущий keyword без учёта регистра.
pub(super) fn strip_leading_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = input.trim_start();
    let (prefix, rest) = trimmed.split_at_checked(keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword)
        || rest.as_bytes().first().is_some_and(|ch| is_word_char(*ch))
    {
        return None;
    }
    Some(rest.trim_start())
}

/// Делит строку по keyword, игнорируя вхождения внутри кавычек.
pub(super) fn split_on_keyword<'a>(input: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let index = TopLevelWordIter::new(input)
        .find(|token| token.word.eq_ignore_ascii_case(keyword))
        .map(|token| token.start)?;
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

    if let Some(after_where) = strip_leading_keyword(rest, "where") {
        let (where_clause, remaining) =
            before_next_clause(after_where, &[TailClause::OrderBy, TailClause::Limit]);
        parsed.where_clause = Some(parse_non_empty_clause("WHERE", where_clause)?);
        rest = remaining;
    }

    if let Some(after_order_by) = strip_prefix_order_by(rest) {
        let (order_by_clause, remaining) = before_next_clause(after_order_by, &[TailClause::Limit]);
        parsed.order_by_clause = Some(parse_non_empty_clause("ORDER BY", order_by_clause)?);
        rest = remaining;
    }

    if let Some(after_limit) = strip_leading_keyword(rest, "limit") {
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

fn strip_prefix_order_by(input: &str) -> Option<&str> {
    let after_order = strip_leading_keyword(input, "order")?;
    strip_leading_keyword(after_order, "by")
}

fn parse_non_empty_clause(name: &str, raw: &str) -> Result<String> {
    let clause = raw.trim();
    if clause.is_empty() {
        bail!("{name} clause must not be empty");
    }
    Ok(clause.to_owned())
}

fn before_next_clause<'a>(input: &'a str, clauses: &[TailClause]) -> (&'a str, &'a str) {
    if let Some(index) = find_next_clause_start(input, clauses) {
        let (before, after) = input.split_at(index);
        return (before.trim_end(), after.trim_start());
    }

    (input, "")
}

fn find_next_clause_start(input: &str, clauses: &[TailClause]) -> Option<usize> {
    let mut previous_word: Option<WordToken<'_>> = None;
    for token in TopLevelWordIter::new(input) {
        if clauses.contains(&TailClause::Limit) && token.word.eq_ignore_ascii_case("limit") {
            return Some(token.start);
        }

        if clauses.contains(&TailClause::OrderBy)
            && let Some(previous) = previous_word
            && previous.word.eq_ignore_ascii_case("order")
            && token.word.eq_ignore_ascii_case("by")
        {
            return Some(previous.start);
        }

        previous_word = Some(token);
    }

    None
}

const fn is_word_char(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || ch == b'_'
}
