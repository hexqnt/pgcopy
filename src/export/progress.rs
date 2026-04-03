use std::path::Path;
use std::time::Duration;

use crate::config::ObjectConfig;
use crate::manifest::ManifestObject;
use crate::progress::{
    ProgressUi, StatusTone, format_duration_compact, object_label, one_line_error,
};

/// Локальный UI-прогресс экспорта (stdout/stderr), отделённый от бизнес-логики.
pub(super) struct ExportProgress {
    ui: ProgressUi,
}

impl ExportProgress {
    pub(super) fn new(objects_count: usize, enabled: bool) -> Self {
        let ui = ProgressUi::new((objects_count + 1) as u64, enabled, "preparing export");
        Self { ui }
    }

    pub(super) fn set_object_running(&self, object: &ObjectConfig) {
        self.ui
            .set_message(format!("exporting {}", object.source_label()));
    }

    pub(super) fn set_object_done(&self, manifest_object: &ManifestObject, elapsed: Duration) {
        self.ui.inc(1);
        self.ui.set_message(format!(
            "done {}.{}",
            manifest_object.source_schema, manifest_object.source_name
        ));
        let label = object_label(&manifest_object.source_schema, &manifest_object.source_name);
        self.ui.print_status_line(
            &label,
            &format!("done ({})", format_duration_compact(elapsed)),
            StatusTone::Success,
        );
    }

    pub(super) fn set_object_error(&self, object: &ObjectConfig, error: &dyn std::error::Error) {
        self.ui
            .set_message(format!("failed {}", object.source_label()));
        let label = object_label(object.source_schema(), object.source_name());
        self.ui.print_status_line(
            &label,
            &format!("[x] error: {}", one_line_error(error)),
            StatusTone::Error,
        );
    }

    pub(super) fn set_bundle_running(&self) {
        self.ui.set_message("packing bundle".to_owned());
    }

    pub(super) fn finish_bundle_done(&self, out_path: &Path, elapsed: Duration) {
        let elapsed = format_duration_compact(elapsed);
        self.ui.inc(1);
        self.ui.print_status_line(
            "[pack]",
            &format!("done ({elapsed}) {}", out_path.display()),
            StatusTone::Success,
        );
        self.ui.finish_with_message(format!(
            "export completed in {elapsed}: {}",
            out_path.display()
        ));
    }

    pub(super) fn finish_bundle_error(&self, out_path: &Path, error: &dyn std::error::Error) {
        self.ui.print_status_line(
            "[pack]",
            &format!(
                "[x] error {}: {}",
                out_path.display(),
                one_line_error(error)
            ),
            StatusTone::Error,
        );
        self.finish_with_error(error);
    }

    pub(super) fn finish_with_error(&self, error: &dyn std::error::Error) {
        self.ui
            .abandon_with_message(format!("export failed: {error}"));
    }
}
