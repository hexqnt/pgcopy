use std::time::{Duration, Instant};

use crate::config::ObjectConfig;
use crate::parallel_workers::WorkerOutcome;

use super::object::ExportedObject;
use super::session::{ExportContext, ExportSession};

/// Результат worker-а экспорта:
/// completed содержит готовые manifest-объекты и время обработки; failure — первая фатальная ошибка.
pub type ExportWorkerOutcome = WorkerOutcome<ObjectConfig, ExportObjectResult>;

/// Результат экспорта одного объекта внутри worker-а.
pub struct ExportObjectResult {
    pub exported: ExportedObject,
    pub elapsed: Duration,
}

/// Выполняет экспорт выделенного набора объектов одним worker-подключением.
pub async fn export_worker(
    context: ExportContext<'_>,
    tasks: Vec<(usize, ObjectConfig)>,
) -> ExportWorkerOutcome {
    let mut completed = Vec::with_capacity(tasks.len());
    let mut session = ExportSession::new(context);

    for (index, object) in tasks {
        let started_at = Instant::now();
        match session.export(index, &object).await {
            Ok(exported) => {
                completed.push((
                    index,
                    ExportObjectResult {
                        exported,
                        elapsed: started_at.elapsed(),
                    },
                ));
            }
            Err(error) => {
                return ExportWorkerOutcome::with_failure(completed, object, error);
            }
        }
    }

    ExportWorkerOutcome::success(completed)
}
