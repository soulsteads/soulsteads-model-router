use crate::{config::ClassTimeoutBudget, sse::ProgressClock};
use std::{future::Future, time::Duration};
use tokio::time::Instant;

pub(super) struct Budget {
    total: Instant,
    attempt: Duration,
    min_retry: Duration,
    first: Option<u64>,
    idle: Option<u64>,
}

impl Budget {
    pub(super) fn new(config: Option<&ClassTimeoutBudget>, streaming: bool) -> Self {
        Self {
            total: Instant::now()
                + Duration::from_secs(config.map_or(120, |budget| budget.total_seconds)),
            attempt: Duration::from_secs(config.map_or(120, |budget| budget.attempt_seconds)),
            min_retry: Duration::from_secs(config.map_or(0, |budget| budget.min_retry_seconds)),
            first: config
                .filter(|_| streaming)
                .and_then(|budget| budget.first_output_seconds),
            idle: config
                .filter(|_| streaming)
                .and_then(|budget| budget.stream_idle_seconds),
        }
    }

    pub(super) fn start_attempt(&self, retry: bool) -> Result<ProgressClock, &'static str> {
        let now = Instant::now();
        let remaining = self.total.saturating_duration_since(now);
        if remaining.is_zero() || (retry && remaining < self.min_retry) {
            return Err("timeout_budget");
        }
        Ok(ProgressClock::new(
            now,
            self.attempt.min(remaining),
            self.first,
            self.idle,
        ))
    }
}

pub(super) async fn wait<T>(
    clock: &ProgressClock,
    future: impl Future<Output = T>,
) -> Result<T, &'static str> {
    if let Some(failure) = clock.failure_at(Instant::now()) {
        return Err(failure.category());
    }
    let result = tokio::time::timeout_at(clock.deadline(), future).await;
    if let Some(failure) = clock.failure_at(Instant::now()) {
        return Err(failure.category());
    }
    result.map_err(|_| "deadline_exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_budgets_and_finite_defaults_bound_attempts() {
        let default = Budget::new(None, true);
        assert_eq!(default.attempt, Duration::from_secs(120));
        assert!(default.total <= Instant::now() + Duration::from_secs(120));
        let config = ClassTimeoutBudget {
            attempt_seconds: 3,
            total_seconds: 5,
            min_retry_seconds: 2,
            first_output_seconds: Some(1),
            stream_idle_seconds: Some(2),
        };
        let budget = Budget::new(Some(&config), true);
        assert_eq!(budget.attempt, Duration::from_secs(3));
        assert_eq!(budget.min_retry, Duration::from_secs(2));
        assert_eq!(budget.first, Some(1));
        assert_eq!(budget.idle, Some(2));
        let nonstream = Budget::new(Some(&config), false);
        assert_eq!((nonstream.first, nonstream.idle), (None, None));
    }

    #[tokio::test]
    async fn refresh_and_retry_share_remaining_total_budget() {
        let budget = Budget {
            total: Instant::now() + Duration::from_millis(60),
            attempt: Duration::from_millis(50),
            min_retry: Duration::ZERO,
            first: None,
            idle: None,
        };
        let first = budget.start_attempt(false).unwrap();
        wait(&first, tokio::time::sleep(Duration::from_millis(25)))
            .await
            .unwrap();
        let retry = budget.start_attempt(true).unwrap();
        assert_eq!(
            retry.deadline(),
            budget.total,
            "retry may not reset the request total"
        );
        assert_eq!(
            wait(&retry, std::future::pending::<()>()).await,
            Err("deadline_exceeded")
        );
        assert!(budget.start_attempt(true).is_err());
    }

    #[tokio::test]
    async fn retry_floor_and_expired_deadline_prevent_future_polling() {
        let mut budget = Budget::new(None, false);
        budget.total = Instant::now() + Duration::from_millis(10);
        budget.min_retry = Duration::from_millis(20);
        assert!(matches!(budget.start_attempt(true), Err("timeout_budget")));
        let clock = ProgressClock::new(
            Instant::now() - Duration::from_secs(2),
            Duration::from_secs(1),
            None,
            None,
        );
        assert_eq!(
            wait(&clock, async {
                panic!("expired budget must not poll auth or upstream")
            })
            .await,
            Err::<(), _>("deadline_exceeded")
        );
    }

    #[tokio::test]
    async fn auth_or_header_cancellation_drops_owned_capacity() {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let clock = ProgressClock::new(Instant::now(), Duration::from_millis(20), None, None);
        let future = async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        };
        assert_eq!(wait(&clock, future).await, Err("deadline_exceeded"));
        assert_eq!(semaphore.available_permits(), 1);
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let clock = Budget::new(None, false).start_attempt(false).unwrap();
            wait(&clock, std::future::pending::<()>()).await
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(semaphore.available_permits(), 1);
    }
}
