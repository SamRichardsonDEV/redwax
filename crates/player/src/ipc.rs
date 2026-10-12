//! mpv's JSON IPC: one line per command, one line per reply or event.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{Error, Result};

pub trait Ipc: Send + Sync + 'static {
    /// Send `{"command": args}` and return the reply's `data`. Errors carry
    /// mpv's `error` string.
    fn command(&self, command_arguments: Vec<Value>) -> impl Future<Output = crate::Result<Value>> + Send;

    fn observe(&self, property: &str) -> impl Future<Output = crate::Result<()>> + Send;
}

pub const OBSERVED: [&str; 12] = [
    "time-pos",
    "duration",
    "pause",
    "playlist-pos",
    "volume",
    "playlist",
    "paused-for-cache",
    "core-idle",
    "loop-file",
    "loop-playlist",
    "mute",
    "eof-reached",
];

/// mpv creates the socket a moment after it starts, so the first connections
/// fail. The budget is generous because a loaded machine is not a failure, and
/// the loop gives up early when the child is already gone.
const CONNECT_RETRY: Duration = Duration::from_millis(20);
const CONNECT_BUDGET: Duration = Duration::from_secs(5);
const COMMAND_BUDGET: Duration = Duration::from_secs(5);

type PendingReplies = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

struct PendingReply<'a> {
    ipc: &'a MpvIpc,
    request_id: u64,
    write_in_progress: bool,
}

impl Drop for PendingReply<'_> {
    fn drop(&mut self) {
        self.ipc
            .pending_replies
            .lock()
            .expect("ipc pending replies")
            .remove(&self.request_id);
        // A partial JSON line cannot share the connection with another command.
        if self.write_in_progress {
            self.ipc.shutdown_now();
        }
    }
}

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe `Ipc`, so the player holds one field whether mpv or a test fake
/// is on the other end. `Ipc` returns `impl Future`, which no trait object can.
pub(crate) trait DynIpc: Send + Sync + 'static {
    fn command(&self, command_arguments: Vec<Value>) -> BoxFuture<'_, Result<Value>>;
}

impl<T: Ipc> DynIpc for T {
    fn command(&self, command_arguments: Vec<Value>) -> BoxFuture<'_, Result<Value>> {
        Box::pin(Ipc::command(self, command_arguments))
    }
}

pub struct MpvIpc {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending_replies: PendingReplies,
    closed: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl MpvIpc {
    /// Connect to `socket_path`, retrying until mpv creates it or `mpv_process`
    /// exits. Every event line mpv sends is forwarded to `event_sender`.
    pub async fn connect(
        socket_path: &Path,
        event_sender: mpsc::UnboundedSender<Value>,
        mut mpv_process: Option<&mut tokio::process::Child>,
    ) -> crate::Result<MpvIpc> {
        let connection_deadline = Instant::now() + CONNECT_BUDGET;
        let ipc_stream = loop {
            match UnixStream::connect(socket_path).await {
                Ok(ipc_stream) => break ipc_stream,
                Err(connection_error) => {
                    if let Some(mpv_process) = mpv_process.as_mut()
                        && let Some(exit_status) = mpv_process.try_wait()?
                    {
                        return Err(Error::Mpv(format!(
                            "mpv exited with status {exit_status} before accepting commands"
                        )));
                    }
                    if Instant::now() >= connection_deadline {
                        return Err(Error::Mpv(format!(
                            "timed out waiting for mpv's IPC socket: {connection_error}"
                        )));
                    }
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
            }
        };
        let (read_half, write_half) = ipc_stream.into_split();
        let pending_replies: PendingReplies = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader_task = tokio::spawn(read_loop(
            read_half,
            Arc::clone(&pending_replies),
            Arc::clone(&closed),
            event_sender,
        ));
        Ok(MpvIpc {
            writer: tokio::sync::Mutex::new(write_half),
            pending_replies,
            closed,
            next_request_id: AtomicU64::new(1),
            reader_task: Mutex::new(Some(reader_task)),
        })
    }

    pub async fn close(&self) {
        self.shutdown_now();
        let _ = self.writer.lock().await.shutdown().await;
    }

    pub(crate) fn shutdown_now(&self) {
        self.closed.store(true, Ordering::Release);
        self.abort_reader();
        fail_pending(&self.pending_replies);
    }

    fn abort_reader(&self) {
        let reader_task = self.reader_task.lock().expect("ipc reader handle").take();
        if let Some(reader_task) = reader_task {
            reader_task.abort();
        }
    }
}

impl Drop for MpvIpc {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}

impl Ipc for MpvIpc {
    async fn command(&self, command_arguments: Vec<Value>) -> crate::Result<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (reply_sender, reply_receiver) = oneshot::channel();
        {
            let mut pending_replies = self.pending_replies.lock().expect("ipc pending replies");
            if self.closed.load(Ordering::Acquire) {
                return Err(Error::Mpv("mpv connection closed".to_string()));
            }
            pending_replies.insert(request_id, reply_sender);
        }
        let mut pending_reply = PendingReply {
            ipc: self,
            request_id,
            write_in_progress: false,
        };
        let command_line = format!(
            "{}\n",
            json!({"command": command_arguments, "request_id": request_id})
        );
        let round_trip = async {
            {
                let mut ipc_writer = self.writer.lock().await;
                if self.closed.load(Ordering::Acquire) {
                    return Err(Error::Mpv("mpv connection closed".to_string()));
                }
                pending_reply.write_in_progress = true;
                ipc_writer.write_all(command_line.as_bytes()).await?;
                ipc_writer.flush().await?;
                pending_reply.write_in_progress = false;
            }
            match reply_receiver.await {
                Ok(command_reply) => command_reply,
                Err(_) => Err(Error::Mpv("mpv connection closed".to_string())),
            }
        };
        match tokio::time::timeout(COMMAND_BUDGET, round_trip).await {
            Ok(command_reply) => command_reply,
            Err(_) => {
                self.shutdown_now();
                Err(Error::Mpv("mpv command timed out".to_string()))
            }
        }
    }

    async fn observe(&self, property_name: &str) -> crate::Result<()> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        Ipc::command(
            self,
            vec![json!("observe_property"), json!(request_id), json!(property_name)],
        )
        .await?;
        Ok(())
    }
}

async fn read_loop(
    ipc_read_half: OwnedReadHalf,
    pending_replies: PendingReplies,
    closed: Arc<AtomicBool>,
    event_sender: mpsc::UnboundedSender<Value>,
) {
    let mut incoming_lines = BufReader::new(ipc_read_half).lines();
    while let Ok(Some(incoming_line)) = incoming_lines.next_line().await {
        let Ok(ipc_message) = serde_json::from_str::<Value>(&incoming_line) else {
            continue;
        };
        if let Some(request_id) = ipc_message.get("request_id").and_then(Value::as_u64) {
            let reply_waiter = pending_replies
                .lock()
                .expect("ipc pending replies")
                .remove(&request_id);
            if let Some(reply_waiter) = reply_waiter {
                let _ = reply_waiter.send(reply_of(&ipc_message));
            }
        } else if ipc_message.get("event").is_some() && event_sender.send(ipc_message).is_err() {
            break;
        }
    }
    closed.store(true, Ordering::Release);
    fail_pending(&pending_replies);
}

fn fail_pending(pending_replies: &PendingReplies) {
    for (_, reply_waiter) in pending_replies.lock().expect("ipc pending replies").drain() {
        let _ = reply_waiter.send(Err(Error::Mpv("mpv connection closed".to_string())));
    }
}

fn reply_of(ipc_message: &Value) -> Result<Value> {
    match ipc_message.get("error").and_then(Value::as_str) {
        Some("success") => Ok(ipc_message.get("data").cloned().unwrap_or(Value::Null)),
        Some(error) => Err(Error::Mpv(error.to_string())),
        None => Err(Error::Mpv("unknown mpv error".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn socket_pair() -> (Arc<MpvIpc>, BufReader<UnixStream>, mpsc::UnboundedReceiver<Value>) {
        let (client, server) = UnixStream::pair().expect("local IPC socket pair");
        let (read_half, write_half) = client.into_split();
        let pending_replies = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let reader_task = tokio::spawn(read_loop(
            read_half,
            Arc::clone(&pending_replies),
            Arc::clone(&closed),
            event_sender,
        ));
        (
            Arc::new(MpvIpc {
                writer: tokio::sync::Mutex::new(write_half),
                pending_replies,
                closed,
                next_request_id: AtomicU64::new(1),
                reader_task: Mutex::new(Some(reader_task)),
            }),
            BufReader::new(server),
            event_receiver,
        )
    }

    #[tokio::test]
    async fn cancelled_reply_waiter_is_removed_and_a_late_reply_does_not_poison_the_next_command() {
        let (ipc, mut server, _events) = socket_pair();
        let first = tokio::spawn({
            let ipc = Arc::clone(&ipc);
            async move { Ipc::command(ipc.as_ref(), vec![json!("first")]).await }
        });
        let mut line = String::new();
        server.read_line(&mut line).await.expect("first command arrives");
        first.abort();
        assert!(
            first
                .await
                .expect_err("first command was cancelled")
                .is_cancelled()
        );
        assert!(
            ipc.pending_replies
                .lock()
                .expect("ipc pending replies")
                .is_empty()
        );
        assert!(!ipc.closed.load(Ordering::Acquire));
        server
            .get_mut()
            .write_all(b"{\"request_id\":1,\"error\":\"success\",\"data\":\"late\"}\n")
            .await
            .expect("late first reply is written");

        let second = tokio::spawn({
            let ipc = Arc::clone(&ipc);
            async move { Ipc::command(ipc.as_ref(), vec![json!("second")]).await }
        });
        line.clear();
        server.read_line(&mut line).await.expect("second command arrives");
        server
            .get_mut()
            .write_all(b"{\"request_id\":2,\"error\":\"success\",\"data\":\"second\"}\n")
            .await
            .expect("second reply is written");
        assert_eq!(
            second
                .await
                .expect("second task completes")
                .expect("second reply"),
            json!("second")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_command_closes_the_event_channel_and_rejects_future_commands() {
        let (ipc, mut server, mut events) = socket_pair();
        let command = tokio::spawn({
            let ipc = Arc::clone(&ipc);
            async move { Ipc::command(ipc.as_ref(), vec![json!("unanswered")]).await }
        });
        let mut line = String::new();
        server.read_line(&mut line).await.expect("command arrives");
        tokio::time::advance(COMMAND_BUDGET).await;
        let error = command
            .await
            .expect("command task completes")
            .expect_err("unanswered command times out");

        assert_eq!(error.to_string(), "mpv: mpv command timed out");
        assert!(
            ipc.pending_replies
                .lock()
                .expect("ipc pending replies")
                .is_empty()
        );
        assert!(events.recv().await.is_none());
        assert!(
            Ipc::command(ipc.as_ref(), vec![json!("after timeout")])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancellation_during_a_partial_write_retires_the_connection() {
        let (ipc, mut server, mut events) = socket_pair();
        let command = tokio::spawn({
            let ipc = Arc::clone(&ipc);
            async move {
                Ipc::command(
                    ipc.as_ref(),
                    vec![json!("large"), json!("x".repeat(2 * 1024 * 1024))],
                )
                .await
            }
        });
        let mut prefix = [0_u8; 64];
        server
            .read_exact(&mut prefix)
            .await
            .expect("partial command reaches the peer");
        command.abort();
        assert!(
            command
                .await
                .expect_err("partial command was cancelled")
                .is_cancelled()
        );

        assert!(ipc.closed.load(Ordering::Acquire));
        assert!(events.recv().await.is_none());
        assert!(Ipc::command(ipc.as_ref(), vec![json!("next")]).await.is_err());
        assert!(
            ipc.pending_replies
                .lock()
                .expect("ipc pending replies")
                .is_empty()
        );
    }
}
