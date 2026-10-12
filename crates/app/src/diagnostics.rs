//! Bounded playback timings with private track references and asynchronous disk writes.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
const PENDING_RECORD_CAPACITY: usize = 256;

type Sink = Arc<dyn Fn(u64, &str, &'static str, &'static str, Duration, Option<&str>) + Send + Sync>;

#[derive(Serialize)]
struct Record {
    timestamp_ms: u64,
    session: String,
    component: &'static str,
    request: u64,
    track: String,
    role: &'static str,
    phase: &'static str,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    dropped_since_last_record: u64,
}

impl Record {
    fn encode(
        session_id: &str,
        component: &'static str,
        request_id: u64,
        video_id: &str,
        timing: (&'static str, &'static str, Duration, Option<&str>),
        dropped_record_count: u64,
    ) -> Option<String> {
        let (role, phase, elapsed_duration, evidence) = timing;
        let track_reference = if video_id.is_empty() {
            String::new()
        } else {
            let mut track_hasher = Sha1::new();
            track_hasher.update(session_id.as_bytes());
            track_hasher.update(video_id.as_bytes());
            hex::encode(track_hasher.finalize())[..12].to_string()
        };
        serde_json::to_string(&Self {
            timestamp_ms: timestamp_millis(),
            session: session_id.to_string(),
            component,
            request: request_id,
            track: track_reference,
            role,
            phase,
            elapsed_ms: elapsed_duration.as_millis().try_into().unwrap_or(u64::MAX),
            evidence: evidence.map(str::to_owned),
            dropped_since_last_record: dropped_record_count,
        })
        .ok()
        .map(|mut serialized_record| {
            serialized_record.push('\n');
            serialized_record
        })
    }
}

enum Message {
    Record(String),
    Shutdown,
}

pub struct TimingLog {
    record_sender: mpsc::Sender<Message>,
    writer_task: JoinHandle<()>,
    session_id: String,
    dropped_record_count: Arc<AtomicU64>,
}

impl TimingLog {
    pub async fn start(log_path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = log_path.parent() {
            tokio::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .await?;
        }
        let log_file = open(&log_path).await?;
        let written_bytes = log_file.metadata().await?.len();
        let (record_sender, records) = mpsc::channel(PENDING_RECORD_CAPACITY);
        let writer_task = tokio::spawn(write_records(log_path, log_file, written_bytes, records));
        Ok(Self {
            record_sender,
            writer_task,
            session_id: format!("{}-{}", std::process::id(), timestamp_millis()),
            dropped_record_count: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn sink(&self, component: &'static str) -> Sink {
        let record_sender = self.record_sender.clone();
        let session_id = self.session_id.clone();
        let dropped_records = Arc::clone(&self.dropped_record_count);
        Arc::new(
            move |request_id, video_id, role, phase, elapsed_duration, evidence| {
                let dropped_record_count = dropped_records.swap(0, Ordering::Relaxed);
                let Some(serialized_record) = Record::encode(
                    &session_id,
                    component,
                    request_id,
                    video_id,
                    (role, phase, elapsed_duration, evidence),
                    dropped_record_count,
                ) else {
                    return;
                };
                if record_sender
                    .try_send(Message::Record(serialized_record))
                    .is_err()
                {
                    dropped_records.fetch_add(dropped_record_count.saturating_add(1), Ordering::Relaxed);
                }
            },
        )
    }

    pub fn stopped(&self) -> bool {
        self.writer_task.is_finished()
    }

    pub async fn shutdown(mut self) {
        let shutdown_result = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = self.record_sender.send(Message::Shutdown).await;
            let _ = (&mut self.writer_task).await;
        })
        .await;
        if shutdown_result.is_err() {
            self.writer_task.abort();
            let _ = (&mut self.writer_task).await;
        }
    }
}

impl Drop for TimingLog {
    fn drop(&mut self) {
        self.writer_task.abort();
    }
}

async fn open(path: &Path) -> io::Result<tokio::fs::File> {
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await?;
    Ok(file)
}

async fn write_records(
    log_path: PathBuf,
    mut log_file: tokio::fs::File,
    mut written_bytes: u64,
    mut records: mpsc::Receiver<Message>,
) {
    while let Some(record_message) = records.recv().await {
        match record_message {
            Message::Record(serialized_record) => {
                if written_bytes.saturating_add(serialized_record.len() as u64) > MAX_LOG_BYTES {
                    if log_file.flush().await.is_err()
                        || tokio::fs::rename(&log_path, log_path.with_extension("jsonl.1"))
                            .await
                            .is_err()
                    {
                        return;
                    }
                    let Ok(next_log_file) = open(&log_path).await else {
                        return;
                    };
                    log_file = next_log_file;
                    written_bytes = 0;
                }
                if log_file.write_all(serialized_record.as_bytes()).await.is_err() {
                    return;
                }
                written_bytes += serialized_record.len() as u64;
                if log_file.flush().await.is_err() {
                    return;
                }
            }
            Message::Shutdown => {
                let _ = log_file.flush().await;
                return;
            }
        }
    }
}

fn timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_optional_and_track_ids_are_not_written() {
        let line = Record::encode(
            "session",
            "resolver",
            7,
            "private-video-id",
            (
                "foreground",
                "cdn.result",
                Duration::ZERO,
                Some("status=403 itag=774 pot=false"),
            ),
            0,
        )
        .expect("the record is serializable");
        assert!(line.contains(r#""evidence":"status=403 itag=774 pot=false""#));
        assert!(!line.contains("private-video-id"));

        let line = Record::encode(
            "session",
            "resolver",
            8,
            "private-video-id",
            ("foreground", "resolve.start", Duration::ZERO, None),
            0,
        )
        .expect("the record is serializable");
        assert!(!line.contains("evidence"));
    }
}
