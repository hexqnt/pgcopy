use std::time::Duration;

use crate::manifest::{Manifest, ManifestObject};
use crate::progress::{
    ProgressUi, StatusTone, format_duration_compact, object_label, one_line_error,
};

/// Локальный UI-прогресс импорта (stdout/stderr), отделённый от бизнес-логики.
pub(super) struct ImportProgress {
    ui: ProgressUi,
}

impl ImportProgress {
    pub(super) fn new(manifest: &Manifest, enabled: bool) -> Self {
        let ui = ProgressUi::new(manifest.objects.len() as u64, enabled, "preparing import");
        Self { ui }
    }

    pub(super) fn set_object_running(&self, object: &ManifestObject) {
        self.ui.set_message(format!(
            "importing {}.{}",
            object.target_schema, object.target_name
        ));
    }

    pub(super) fn set_object_done(
        &self,
        object: &ManifestObject,
        inserted: u64,
        elapsed: Duration,
    ) {
        let elapsed = format_duration_compact(elapsed);
        self.ui.inc(1);
        self.ui.set_message(format!(
            "done {}.{}",
            object.target_schema, object.target_name
        ));
        let label = object_label(&object.target_schema, &object.target_name);
        self.ui.print_status_line(
            &label,
            &format!("done ({inserted} rows, {elapsed})"),
            StatusTone::Success,
        );
    }

    pub(super) fn set_object_error(&self, object: &ManifestObject, error: &dyn std::error::Error) {
        self.ui.set_message(format!(
            "failed {}.{}",
            object.target_schema, object.target_name
        ));
        let label = object_label(&object.target_schema, &object.target_name);
        self.ui.print_status_line(
            &label,
            &format!("[x] error: {}", one_line_error(error)),
            StatusTone::Error,
        );
    }

    pub(super) fn finish_done(&self, total_rows: u64, elapsed: Duration) {
        let elapsed = format_duration_compact(elapsed);
        self.ui.print_status_line(
            "[import]",
            &format!("done ({total_rows} rows, {elapsed})"),
            StatusTone::Success,
        );
        self.ui
            .finish_with_message(format!("import completed in {elapsed}: {total_rows} rows"));
    }

    pub(super) fn finish_with_error(&self, error: &dyn std::error::Error) {
        self.ui
            .abandon_with_message(format!("import failed: {error}"));
    }
}
