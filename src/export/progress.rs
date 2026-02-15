use std::path::Path;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::config::{Config, ObjectConfig};
use crate::manifest::ManifestObject;

/// Локальный UI-прогресс экспорта (stdout/stderr), отделённый от бизнес-логики.
pub(super) struct ExportProgress {
    _multi: MultiProgress,
    overall: ProgressBar,
    object_lines: Vec<ProgressBar>,
    bundle_line: ProgressBar,
}

impl ExportProgress {
    pub(super) fn new(config: &Config, enabled: bool) -> Self {
        let multi = if enabled {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };

        let overall = multi.add(ProgressBar::new((config.objects.len() + 1) as u64));
        overall.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>2}/{len:2} {msg}",
            )
            .expect("valid overall progress template")
            .progress_chars("=>-"),
        );
        overall.set_message("preparing export");

        let object_lines = config
            .objects
            .iter()
            .map(|object| {
                let line = multi.add(ProgressBar::new_spinner());
                line.set_style(object_queued_style());
                line.set_prefix(format!(
                    "{}.{}",
                    object.select.source_schema, object.select.source_name
                ));
                line.set_message("[ ] queued".to_owned());
                line
            })
            .collect::<Vec<_>>();

        let bundle_line = multi.add(ProgressBar::new_spinner());
        bundle_line.set_style(bundle_queued_style());
        bundle_line.set_prefix("[pack]".to_owned());
        bundle_line.set_message("[ ] bundle queued".to_owned());

        Self {
            _multi: multi,
            overall,
            object_lines,
            bundle_line,
        }
    }

    pub(super) fn set_object_running(&self, index: usize, object: &ObjectConfig) {
        let line = &self.object_lines[index];
        line.set_style(object_running_style());
        line.enable_steady_tick(Duration::from_millis(100));
        line.set_message("[~] exporting".to_owned());
        self.overall.set_message(format!(
            "exporting {}.{}",
            object.select.source_schema, object.select.source_name
        ));
    }

    pub(super) fn set_object_done(&self, index: usize, manifest_object: &ManifestObject) {
        let line = &self.object_lines[index];
        line.set_style(object_done_style());
        line.finish_with_message("[v] done".to_owned());
        self.overall.inc(1);
        self.overall.set_message(format!(
            "done {}.{}",
            manifest_object.source_schema, manifest_object.source_name
        ));
    }

    pub(super) fn set_object_error(
        &self,
        index: usize,
        object: &ObjectConfig,
        error: &dyn std::error::Error,
    ) {
        let line = &self.object_lines[index];
        line.set_style(object_error_style());
        line.abandon_with_message(format!("[x] error: {error}"));
        self.overall.set_message(format!(
            "failed {}.{}",
            object.select.source_schema, object.select.source_name
        ));
    }

    pub(super) fn set_bundle_running(&self, out_path: &Path) {
        self.bundle_line.set_style(bundle_running_style());
        self.bundle_line
            .enable_steady_tick(Duration::from_millis(100));
        self.bundle_line
            .set_message(format!("[~] bundle {} packing", out_path.display()));
        self.overall.set_message("packing bundle");
    }

    pub(super) fn finish_bundle_done(&self, out_path: &Path) {
        self.bundle_line.set_style(bundle_done_style());
        self.bundle_line
            .finish_with_message(format!("[v] bundle {} done", out_path.display()));
        self.overall.inc(1);
        self.overall
            .finish_with_message(format!("export completed: {}", out_path.display()));
    }

    pub(super) fn finish_bundle_error(&self, out_path: &Path, error: &dyn std::error::Error) {
        self.bundle_line.set_style(bundle_error_style());
        self.bundle_line.abandon_with_message(format!(
            "[x] bundle {} error: {}",
            out_path.display(),
            error
        ));
        self.finish_with_error(error);
    }

    pub(super) fn finish_with_error(&self, error: &dyn std::error::Error) {
        self.overall
            .abandon_with_message(format!("export failed: {error}"));
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

fn bundle_queued_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:>6.dim} {msg.dim}")
        .expect("valid queued bundle status template")
}

fn bundle_running_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:>6.yellow} {spinner:.yellow} {msg.yellow}")
        .expect("valid running bundle status template")
}

fn bundle_done_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:>6.green} {msg.green}")
        .expect("valid done bundle status template")
}

fn bundle_error_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:>6.red} {msg.red}")
        .expect("valid error bundle status template")
}
