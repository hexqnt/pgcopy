use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

const OBJECT_LABEL_WIDTH: usize = 42;
const STATUS_WIDTH: usize = 40;

#[derive(Clone, Copy)]
pub(crate) enum StatusTone {
    Success,
    Error,
}

/// Общий UI-слой прогресса поверх `indicatif::ProgressBar`.
pub(crate) struct ProgressUi {
    bar: ProgressBar,
}

impl ProgressUi {
    pub(crate) fn new(total: u64, enabled: bool, initial_message: &str) -> Self {
        let draw_target = if enabled {
            ProgressDrawTarget::stderr_with_hz(8)
        } else {
            ProgressDrawTarget::hidden()
        };

        let bar = ProgressBar::with_draw_target(Some(total), draw_target);
        bar.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] {wide_bar:.cyan/blue} {pos:>3}/{len:3} {msg:24!}",
            )
            .expect("valid overall progress template")
            .progress_chars("=>-"),
        );
        bar.set_message(initial_message.to_owned());

        Self { bar }
    }

    pub(crate) fn set_message(&self, message: impl Into<String>) {
        self.bar.set_message(message.into());
    }

    pub(crate) fn inc(&self, delta: u64) {
        self.bar.inc(delta);
    }

    pub(crate) fn print_status_line(&self, label: &str, status: &str, tone: StatusTone) {
        self.bar.println(format_status_line(label, status, tone));
    }

    pub(crate) fn finish_with_message(&self, message: impl Into<String>) {
        self.bar.finish_with_message(message.into());
    }

    pub(crate) fn abandon_with_message(&self, message: impl Into<String>) {
        self.bar.abandon_with_message(message.into());
    }
}

pub(crate) fn object_label(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

pub(crate) fn one_line_error(error: &dyn std::error::Error) -> String {
    error.to_string().replace('\n', " | ")
}

pub(crate) fn format_duration_compact(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        let millis = elapsed.as_millis();
        let millis = if millis == 0 && !elapsed.is_zero() {
            1
        } else {
            millis
        };
        return format!("{millis}ms");
    }

    let total_seconds = elapsed.as_secs();
    if total_seconds < 60 {
        return format!("{total_seconds}s");
    }

    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        return format!("{hours}h {minutes}m {seconds}s");
    }

    format!("{minutes}m {seconds}s")
}

fn format_status_line(label: &str, status: &str, tone: StatusTone) -> String {
    let status = truncate_text(status, STATUS_WIDTH);
    let status = colorize_status(&status, tone);
    format!(
        "{:<width$} {}",
        truncate_text(label, OBJECT_LABEL_WIDTH),
        status,
        width = OBJECT_LABEL_WIDTH
    )
}

fn colorize_status(value: &str, tone: StatusTone) -> String {
    const RESET: &str = "\x1b[0m";
    match tone {
        StatusTone::Success => format!("\x1b[32m{value}{RESET}"),
        StatusTone::Error => format!("\x1b[31m{value}{RESET}"),
    }
}

fn truncate_text(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if len <= width {
        return value.to_owned();
    }

    if width <= 3 {
        return value.chars().take(width).collect();
    }

    let mut out = value.chars().take(width - 3).collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_duration_compact;

    #[test]
    fn formats_duration_under_one_second_as_milliseconds() {
        assert_eq!(format_duration_compact(Duration::from_millis(850)), "850ms");
    }

    #[test]
    fn formats_non_zero_sub_millisecond_duration_as_one_millisecond() {
        assert_eq!(format_duration_compact(Duration::from_nanos(1)), "1ms");
    }

    #[test]
    fn formats_duration_under_one_minute_as_seconds() {
        assert_eq!(format_duration_compact(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn formats_one_second_boundary_as_seconds() {
        assert_eq!(format_duration_compact(Duration::from_secs(1)), "1s");
    }

    #[test]
    fn formats_duration_under_one_hour_as_minutes_and_seconds() {
        assert_eq!(format_duration_compact(Duration::from_secs(134)), "2m 14s");
    }

    #[test]
    fn formats_one_minute_boundary_as_minutes_and_seconds() {
        assert_eq!(format_duration_compact(Duration::from_secs(60)), "1m 0s");
    }

    #[test]
    fn formats_duration_one_hour_or_more_as_hours_minutes_seconds() {
        assert_eq!(
            format_duration_compact(Duration::from_secs(3_723)),
            "1h 2m 3s"
        );
    }

    #[test]
    fn formats_one_hour_boundary_as_hours_minutes_seconds() {
        assert_eq!(
            format_duration_compact(Duration::from_secs(3_600)),
            "1h 0m 0s"
        );
    }
}
