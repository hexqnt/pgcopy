use anyhow::{Context, Result, bail};

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

    if let Some(after_where) = strip_prefix_where(rest) {
        let (where_clause, remaining) = split_clause(before_next_clause(
            after_where,
            &[TailClause::OrderBy, TailClause::Limit],
        ));
        parsed.where_clause = Some(parse_non_empty_clause("WHERE", where_clause)?);
        rest = remaining;
    }

    if let Some(after_order_by) = strip_prefix_order_by(rest) {
        let (order_by_clause, remaining) =
            split_clause(before_next_clause(after_order_by, &[TailClause::Limit]));
        parsed.order_by_clause = Some(parse_non_empty_clause("ORDER BY", order_by_clause)?);
        rest = remaining;
    }

    if let Some(after_limit) = strip_prefix_limit(rest) {
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

fn strip_prefix_where(input: &str) -> Option<&str> {
    strip_leading_keyword(input, "where")
}

fn strip_prefix_order_by(input: &str) -> Option<&str> {
    let after_order = strip_leading_keyword(input, "order")?;
    strip_leading_keyword(after_order, "by")
}

fn strip_prefix_limit(input: &str) -> Option<&str> {
    strip_leading_keyword(input, "limit")
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

#[derive(Clone, Copy)]
struct WordToken<'a> {
    word: &'a str,
    start: usize,
}

struct TopLevelWordIter<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
    paren_depth: usize,
    in_single_quote: bool,
    in_double_quote: bool,
}

impl<'a> TopLevelWordIter<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            index: 0,
            paren_depth: 0,
            in_single_quote: false,
            in_double_quote: false,
        }
    }
}

impl<'a> Iterator for TopLevelWordIter<'a> {
    type Item = WordToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.bytes.len() {
            let ch = self.bytes[self.index];

            if self.in_single_quote {
                if ch == b'\'' {
                    if self.index + 1 < self.bytes.len() && self.bytes[self.index + 1] == b'\'' {
                        self.index += 2;
                        continue;
                    }
                    self.in_single_quote = false;
                }
                self.index += 1;
                continue;
            }

            if self.in_double_quote {
                if ch == b'"' {
                    if self.index + 1 < self.bytes.len() && self.bytes[self.index + 1] == b'"' {
                        self.index += 2;
                        continue;
                    }
                    self.in_double_quote = false;
                }
                self.index += 1;
                continue;
            }

            match ch {
                b'\'' => {
                    self.in_single_quote = true;
                    self.index += 1;
                    continue;
                }
                b'"' => {
                    self.in_double_quote = true;
                    self.index += 1;
                    continue;
                }
                b'(' => {
                    self.paren_depth += 1;
                    self.index += 1;
                    continue;
                }
                b')' => {
                    self.paren_depth = self.paren_depth.saturating_sub(1);
                    self.index += 1;
                    continue;
                }
                _ => {}
            }

            if self.paren_depth == 0 && is_word_char(ch) {
                let start = self.index;
                self.index += 1;
                while self.index < self.bytes.len() && is_word_char(self.bytes[self.index]) {
                    self.index += 1;
                }
                return Some(WordToken {
                    word: &self.input[start..self.index],
                    start,
                });
            }

            self.index += 1;
        }

        None
    }
}
