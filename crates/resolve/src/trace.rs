//! Phase timings owned by one resolver and correlated through its async work.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub type RuntimeSink = dyn Fn(u64, &str, &'static str, &'static str, Duration, Option<&str>) + Send + Sync;

type CliLog = Arc<Mutex<Vec<(String, Duration)>>>;

struct ResolutionRequestContext {
    request_id: u64,
    video_id: String,
    role: &'static str,
    runtime_sink: Option<Arc<RuntimeSink>>,
    cli_log: Option<CliLog>,
}

tokio::task_local! {
    static REQUEST: ResolutionRequestContext;
}

pub(crate) struct Trace {
    runtime_sink: RwLock<Option<Arc<RuntimeSink>>>,
    next_request_id: AtomicU64,
    cli_enabled: AtomicBool,
    cli_log: CliLog,
}

impl Default for Trace {
    fn default() -> Trace {
        Trace {
            runtime_sink: RwLock::new(None),
            next_request_id: AtomicU64::new(1),
            cli_enabled: AtomicBool::new(false),
            cli_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Trace {
    /// The callback receives fixed phases, timings and privacy-safe evidence.
    /// It must return promptly because resolution awaits no logging task.
    pub fn set_runtime_sink(&self, runtime_sink: Option<Arc<RuntimeSink>>) {
        *self
            .runtime_sink
            .write()
            .expect("the runtime sink lock is never held across a callback") = runtime_sink;
    }

    pub fn enable_cli(&self) {
        self.cli_enabled.store(true, Ordering::Relaxed);
    }

    pub fn take_cli(&self) -> Vec<(String, Duration)> {
        std::mem::take(
            &mut *self
                .cli_log
                .lock()
                .expect("the trace lock is never held across an await"),
        )
    }

    /// Correlate nested phases with this resolver and request.
    pub async fn scope<T>(&self, video_id: &str, role: &'static str, work: impl Future<Output = T>) -> T {
        let runtime_sink = self
            .runtime_sink
            .read()
            .expect("the runtime sink lock is never held across a callback")
            .clone();
        let cli_log = self
            .cli_enabled
            .load(Ordering::Relaxed)
            .then(|| Arc::clone(&self.cli_log));
        if runtime_sink.is_none() && cli_log.is_none() {
            return work.await;
        }
        let request_context = ResolutionRequestContext {
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            video_id: video_id.to_owned(),
            role,
            runtime_sink,
            cli_log,
        };
        REQUEST
            .scope(request_context, async {
                record_runtime("resolve.start", Duration::ZERO);
                timed("resolve.total", work).await
            })
            .await
    }
}

pub fn record_runtime(phase: &'static str, elapsed_time: Duration) {
    let _ = REQUEST.try_with(|request| {
        if let Some(runtime_sink) = &request.runtime_sink {
            runtime_sink(
                request.request_id,
                &request.video_id,
                request.role,
                phase,
                elapsed_time,
                None,
            );
        }
    });
}

pub fn record_runtime_evidence(phase: &'static str, elapsed_time: Duration, evidence: &str) {
    let _ = REQUEST.try_with(|request| {
        if let Some(runtime_sink) = &request.runtime_sink {
            runtime_sink(
                request.request_id,
                &request.video_id,
                request.role,
                phase,
                elapsed_time,
                Some(evidence),
            );
        }
    });
}

fn record(phase: impl Into<String>, elapsed_time: Duration) {
    let _ = REQUEST.try_with(|request| {
        if let Some(cli_log) = &request.cli_log {
            cli_log
                .lock()
                .expect("the trace lock is never held across an await")
                .push((phase.into(), elapsed_time));
        }
    });
}

/// Times `work` within the active resolver request.
pub async fn timed<T>(phase: &'static str, work: impl Future<Output = T>) -> T {
    if REQUEST.try_with(|_| ()).is_err() {
        return work.await;
    }
    let phase_started_at = Instant::now();
    let work_output = work.await;
    let elapsed_time = phase_started_at.elapsed();
    record(phase, elapsed_time);
    record_runtime(phase, elapsed_time);
    work_output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn separate_traces_keep_concurrent_request_context_and_sinks() {
        let first_events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&first_events);
        let first_sink: Arc<RuntimeSink> = Arc::new(move |id, video_id, role, phase, _, _| {
            recorded
                .lock()
                .expect("the test event lock is never held across an await")
                .push((id, video_id.to_owned(), role, phase));
        });
        let second_events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&second_events);
        let second_sink: Arc<RuntimeSink> = Arc::new(move |id, video_id, role, phase, _, _| {
            recorded
                .lock()
                .expect("the test event lock is never held across an await")
                .push((id, video_id.to_owned(), role, phase));
        });
        let first = Trace::default();
        first.set_runtime_sink(Some(first_sink));
        let second = Trace::default();
        second.set_runtime_sink(Some(second_sink));
        let work = || timed("player request", async { tokio::task::yield_now().await });

        tokio::join!(
            first.scope("selected", "foreground", work()),
            second.scope("upcoming", "prefetch", work())
        );

        assert!(
            first_events
                .lock()
                .expect("the first event lock is available")
                .iter()
                .all(|(_, video_id, _, _)| video_id == "selected")
        );
        assert!(
            second_events
                .lock()
                .expect("the second event lock is available")
                .iter()
                .all(|(_, video_id, _, _)| video_id == "upcoming")
        );
    }
}
