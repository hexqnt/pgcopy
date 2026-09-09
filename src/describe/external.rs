use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use super::OutputFormat;

const MARKDOWN_FORMATTERS: &[Formatter] = &[
    Formatter {
        executable: "markdownlint-cli2",
        arguments_before_path: &["--fix"],
        output_source: OutputSource::File,
    },
    Formatter {
        executable: "markdownlint",
        arguments_before_path: &["--fix"],
        output_source: OutputSource::File,
    },
    Formatter {
        executable: "prettier",
        arguments_before_path: &["--write"],
        output_source: OutputSource::File,
    },
];

const SQL_FORMATTERS: &[Formatter] = &[
    Formatter {
        executable: "pg_format",
        arguments_before_path: &[],
        output_source: OutputSource::Stdout,
    },
    Formatter {
        executable: "sqlfluff",
        arguments_before_path: &["fix", "--dialect", "postgres", "--disable-progress-bar"],
        output_source: OutputSource::File,
    },
];

const JSON_FORMATTERS: &[Formatter] = &[
    Formatter {
        executable: "biome",
        arguments_before_path: &["format", "--write"],
        output_source: OutputSource::File,
    },
    Formatter {
        executable: "prettier",
        arguments_before_path: &["--write"],
        output_source: OutputSource::File,
    },
    Formatter {
        executable: "jq",
        arguments_before_path: &["."],
        output_source: OutputSource::Stdout,
    },
];

#[derive(Debug, Clone, Copy)]
enum OutputSource {
    File,
    Stdout,
}

#[derive(Debug, Clone, Copy)]
struct Formatter {
    executable: &'static str,
    arguments_before_path: &'static [&'static str],
    output_source: OutputSource,
}

pub(super) fn format(
    contents: String,
    output_format: OutputFormat,
    formatter_dir: &Path,
) -> Result<String> {
    let (suffix, formatters) = match output_format {
        OutputFormat::Markdown => (".md", MARKDOWN_FORMATTERS),
        OutputFormat::Sql => (".sql", SQL_FORMATTERS),
        OutputFormat::Json => (".json", JSON_FORMATTERS),
    };
    std::fs::create_dir_all(formatter_dir).with_context(|| {
        format!(
            "failed to create formatter directory {}",
            formatter_dir.display()
        )
    })?;
    let temporary = tempfile::Builder::new()
        .prefix("pgcopy-describe-format-")
        .suffix(suffix)
        .tempfile_in(formatter_dir)
        .with_context(|| {
            format!(
                "failed to create temporary formatter file in {}",
                formatter_dir.display()
            )
        })?;
    let mut failures = Vec::new();
    for formatter in formatters {
        std::fs::write(temporary.path(), contents.as_bytes())
            .context("failed to stage schema context for external formatter")?;
        let result = run_formatter(*formatter, temporary.path());
        match result {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                failures.push(format!("{}: {error}", formatter.executable));
            }
            Ok(process_output) => {
                if !process_output.status.success() {
                    failures.push(format_process_failure(*formatter, &process_output));
                    continue;
                }
                match read_formatted_output(*formatter, temporary.path(), process_output) {
                    Ok(formatted) => {
                        if let Err(error) = validate_formatted_output(
                            &formatted,
                            output_format,
                            formatter.executable,
                        ) {
                            failures.push(error.to_string());
                            continue;
                        }
                        tracing::info!(
                            formatter = formatter.executable,
                            "formatted schema context with external formatter"
                        );
                        return Ok(formatted);
                    }
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }
    }

    if failures.is_empty() {
        let supported = formatters
            .iter()
            .map(|formatter| formatter.executable)
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            "no supported external formatter found for {} (tried: {supported}); keeping built-in formatting",
            output_format.as_str()
        );
    } else {
        tracing::warn!(
            "external formatting failed for {}; keeping built-in formatting: {}",
            output_format.as_str(),
            failures.join("; ")
        );
    }
    Ok(contents)
}

fn read_formatted_output(
    formatter: Formatter,
    path: &Path,
    process_output: Output,
) -> Result<String> {
    match formatter.output_source {
        OutputSource::File => std::fs::read_to_string(path).with_context(|| {
            format!(
                "failed to read output from external formatter {}",
                formatter.executable
            )
        }),
        OutputSource::Stdout => String::from_utf8(process_output.stdout).with_context(|| {
            format!(
                "external formatter {} produced non-UTF-8 output",
                formatter.executable
            )
        }),
    }
}

fn format_process_failure(formatter: Formatter, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    let detail = truncate_detail(&detail);
    if detail.is_empty() {
        format!("{} exited with {}", formatter.executable, output.status)
    } else {
        format!(
            "{} exited with {}: {}",
            formatter.executable, output.status, detail
        )
    }
}

fn truncate_detail(detail: &str) -> String {
    const MAX_CHARS: usize = 240;

    let mut chars = detail.chars();
    let mut truncated = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn validate_formatted_output(
    contents: &str,
    output_format: OutputFormat,
    formatter: &str,
) -> Result<()> {
    if contents.trim().is_empty() {
        bail!("external formatter {formatter} produced empty output");
    }
    if output_format == OutputFormat::Json {
        serde_json::from_str::<serde_json::Value>(contents)
            .with_context(|| format!("external formatter {formatter} produced invalid JSON"))?;
    }
    Ok(())
}

fn run_formatter(formatter: Formatter, path: &Path) -> std::io::Result<Output> {
    Command::new(formatter.executable)
        .args(formatter.arguments_before_path)
        .arg(path)
        .output()
}

#[cfg(test)]
mod tests {
    use super::{
        JSON_FORMATTERS, MARKDOWN_FORMATTERS, SQL_FORMATTERS, truncate_detail,
        validate_formatted_output,
    };
    use crate::describe::OutputFormat;

    #[test]
    fn formatter_priorities_are_stable() {
        assert_eq!(MARKDOWN_FORMATTERS[0].executable, "markdownlint-cli2");
        assert_eq!(SQL_FORMATTERS[0].executable, "pg_format");
        assert_eq!(JSON_FORMATTERS[0].executable, "biome");
    }

    #[test]
    fn rejects_empty_and_invalid_formatter_output() {
        assert!(validate_formatted_output("", OutputFormat::Sql, "test").is_err());
        assert!(validate_formatted_output("not json", OutputFormat::Json, "test").is_err());
        assert!(validate_formatted_output("{}\n", OutputFormat::Json, "test").is_ok());
    }

    #[test]
    fn truncates_formatter_diagnostics_on_character_boundary() {
        let raw = "я".repeat(300);
        let detail = truncate_detail(&raw);
        assert_eq!(detail.chars().count(), 241);
        assert!(detail.ends_with('…'));
    }
}
