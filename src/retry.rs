use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;

/// Переопределения политики на границе CLI/TOML; отсутствие поля сохраняет приоритет ниже.
#[derive(Debug, Default, Clone, Copy, Args, Deserialize)]
pub(crate) struct RetryOptions {
    #[arg(
        long,
        value_name = "N",
        help = "Total attempts per export object, including the first (default: 3; 1 disables retries)"
    )]
    attempts: Option<NonZeroU32>,
    #[arg(
        long,
        value_name = "MS",
        help = "Delay between export attempts in milliseconds (default: 1000)"
    )]
    retry_delay_ms: Option<u32>,
    #[arg(
        long,
        value_name = "MS",
        help = "Timeout per export attempt in milliseconds, including connection setup (default: 0, unlimited)"
    )]
    attempt_timeout_ms: Option<u32>,
}

impl RetryOptions {
    pub(crate) fn resolve(self, fallback: RetryPolicy) -> RetryPolicy {
        RetryPolicy {
            attempts: self.attempts.unwrap_or(fallback.attempts),
            delay: self
                .retry_delay_ms
                .map_or(fallback.delay, |ms| Duration::from_millis(u64::from(ms))),
            timeout: self.attempt_timeout_ms.map_or(fallback.timeout, |ms| {
                NonZeroU32::new(ms).map(|ms| Duration::from_millis(u64::from(ms.get())))
            }),
        }
    }
}

/// Нормализованная политика: хотя бы одна попытка, нулевой таймаут представлен как `None`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub(crate) attempts: NonZeroU32,
    pub(crate) delay: Duration,
    pub(crate) timeout: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: NonZeroU32::new(3).expect("three is nonzero"),
            delay: Duration::from_secs(1),
            timeout: None,
        }
    }
}

impl RetryPolicy {
    /// Отмена future попытки должна освобождать её соединение и временные файлы.
    pub(crate) async fn run<T, Fut>(
        self,
        label: &str,
        mut operation: impl FnMut() -> Fut,
    ) -> Result<T>
    where
        Fut: Future<Output = Result<T>>,
    {
        for attempt in 1..=self.attempts.get() {
            let result = if let Some(timeout) = self.timeout {
                tokio::time::timeout(timeout, operation())
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "export attempt timed out after {} ms",
                            timeout.as_millis()
                        ))
                    })
            } else {
                operation().await
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error) if attempt == self.attempts.get() => {
                    return Err(error).with_context(|| {
                        format!("export object {label} failed after {attempt} attempt(s)")
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        object = label,
                        attempt,
                        max_attempts = self.attempts.get(),
                        retry_delay_ms = self.delay.as_millis(),
                        error = %format_args!("{error:#}"),
                        "export attempt failed; retrying"
                    );
                    tokio::time::sleep(self.delay).await;
                }
            }
        }
        unreachable!("attempt count is nonzero")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::{pending, ready};

    use super::{RetryOptions, RetryPolicy};
    use anyhow::{Result, anyhow};
    use tokio::time::{Duration, Instant};

    fn policy(attempts: u32) -> RetryPolicy {
        RetryPolicy {
            attempts: attempts.try_into().expect("nonzero attempts"),
            delay: Duration::from_secs(2),
            timeout: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stops_on_success_and_waits_only_between_failures() {
        for failures in 0_u32..3 {
            let mut attempts = 0;
            let started = Instant::now();
            let result = policy(3)
                .run("public.items", || {
                    attempts += 1;
                    ready(if attempts <= failures {
                        Err(anyhow!("transient failure"))
                    } else {
                        Ok(42)
                    })
                })
                .await
                .expect("eventual success");
            assert_eq!(result, 42);
            assert_eq!(attempts, failures + 1);
            assert_eq!(
                started.elapsed(),
                Duration::from_secs(u64::from(failures) * 2)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exhausts_limit_and_preserves_last_error_without_final_delay() {
        for limit in [1, 3] {
            let mut attempts = 0;
            let started = Instant::now();
            let error = policy(limit)
                .run("public.items", || {
                    attempts += 1;
                    ready(Err::<(), _>(anyhow!("failure {attempts}")))
                })
                .await
                .expect_err("attempt limit must apply");
            assert_eq!(attempts, limit);
            assert_eq!(
                started.elapsed(),
                Duration::from_secs(u64::from(limit - 1) * 2)
            );
            assert!(
                error
                    .to_string()
                    .contains(&format!("public.items failed after {limit} attempt(s)"))
            );
            assert_eq!(error.root_cause().to_string(), format!("failure {limit}"));
        }
    }

    struct DropCounter<'a>(&'a Cell<u32>);

    impl Drop for DropCounter<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_cancels_each_attempt_and_applies_delay() {
        let drops = Cell::new(0);
        let started = Instant::now();
        let retry = RetryPolicy {
            timeout: Some(Duration::from_secs(5)),
            ..policy(3)
        };
        let error = retry
            .run("public.slow", || async {
                let _guard = DropCounter(&drops);
                pending::<Result<()>>().await
            })
            .await
            .expect_err("timeout must stop each attempt");
        assert_eq!(drops.get(), 3);
        assert_eq!(started.elapsed(), Duration::from_secs(19));
        assert!(
            error
                .root_cause()
                .to_string()
                .contains("timed out after 5000 ms")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn can_succeed_after_timeout() {
        let mut attempts = 0;
        let retry = RetryPolicy {
            timeout: Some(Duration::from_secs(5)),
            ..policy(3)
        };
        retry
            .run("public.slow", || {
                attempts += 1;
                let attempt = attempts;
                async move {
                    if attempt == 1 {
                        pending::<()>().await;
                    }
                    Ok(())
                }
            })
            .await
            .expect("second attempt succeeds");
        assert_eq!(attempts, 2);
    }

    #[test]
    fn overrides_only_present_fields_and_zero_disables_timeout() {
        let configured: RetryOptions =
            toml::from_str("attempts = 5\nretry_delay_ms = 20\nattempt_timeout_ms = 500").unwrap();
        let fallback = configured.resolve(RetryPolicy::default());
        let override_options: RetryOptions =
            toml::from_str("attempts = 2\nattempt_timeout_ms = 0").unwrap();
        let resolved = override_options.resolve(fallback);
        assert_eq!(resolved.attempts.get(), 2);
        assert_eq!(resolved.delay, Duration::from_millis(20));
        assert_eq!(resolved.timeout, None);
        assert_eq!(
            RetryOptions::default().resolve(fallback).timeout,
            Some(Duration::from_millis(500))
        );
    }
}
