use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::manifest::{Manifest, ManifestObject};

/// Локальный UI-прогресс импорта (stdout/stderr), отделённый от бизнес-логики.
pub(super) struct ImportProgress {
    _multi: MultiProgress,
    overall: ProgressBar,
    object_lines: Vec<ProgressBar>,
}

impl ImportProgress {
    pub(super) fn new(manifest: &Manifest, enabled: bool) -> Self {
        let multi = if enabled {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };

        let overall = multi.add(ProgressBar::new(manifest.objects.len() as u64));
        overall.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>2}/{len:2} {msg}",
            )
            .expect("valid overall progress template")
            .progress_chars("=>-"),
        );
        overall.set_message("preparing import");

        let object_lines = manifest
            .objects
            .iter()
            .map(|object| {
                let line = multi.add(ProgressBar::new_spinner());
                line.set_style(object_queued_style());
                line.set_prefix(format!("{}.{}", object.target_schema, object.target_name));
                line.set_message("[ ] queued".to_owned());
                line
            })
            .collect::<Vec<_>>();

        Self {
            _multi: multi,
            overall,
            object_lines,
        }
    }

    pub(super) fn set_object_running(&self, index: usize, object: &ManifestObject) {
        let line = &self.object_lines[index];
        line.set_style(object_running_style());
        line.enable_steady_tick(Duration::from_millis(100));
        line.set_message("[~] importing".to_owned());
        self.overall.set_message(format!(
            "importing {}.{}",
            object.target_schema, object.target_name
        ));
    }

    pub(super) fn set_object_done(&self, index: usize, object: &ManifestObject, inserted: u64) {
        let line = &self.object_lines[index];
        line.set_style(object_done_style());
        line.finish_with_message(format!("[v] done ({inserted} rows)"));
        self.overall.inc(1);
        self.overall.set_message(format!(
            "done {}.{}",
            object.target_schema, object.target_name
        ));
    }

    pub(super) fn set_object_error(
        &self,
        index: usize,
        object: &ManifestObject,
        error: &dyn std::error::Error,
    ) {
        let line = &self.object_lines[index];
        line.set_style(object_error_style());
        line.abandon_with_message(format!("[x] error: {error}"));
        self.overall.set_message(format!(
            "failed {}.{}",
            object.target_schema, object.target_name
        ));
    }

    pub(super) fn finish_done(&self, total_rows: u64) {
        self.overall
            .finish_with_message(format!("import completed: {} rows", total_rows));
    }

    pub(super) fn finish_with_error(&self, error: &dyn std::error::Error) {
        self.overall
            .abandon_with_message(format!("import failed: {error}"));
    }
}

fn object_queued_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.dim} {msg.dim}")
        .expect("valid queued object status template")
}

fn object_running_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.cyan} {spinner:.cyan} {msg.cyan}")
        .expect("valid running object status template")
}

fn object_done_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.green} {msg.green}")
        .expect("valid done object status template")
}

fn object_error_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.red} {msg.red}")
        .expect("valid error object status template")
}
