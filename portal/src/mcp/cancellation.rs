//! Bounded best-effort MCP cancellation; never blocks the caller's Drop path.
use super::ownership::ProcessOwner;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, Mutex};

pub(super) fn start(
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
    owner: Option<Arc<ProcessOwner>>,
    alive: Arc<AtomicBool>,
) -> (mpsc::Sender<u64>, tokio::task::JoinHandle<()>) {
    let (sender, mut receiver) = mpsc::channel::<u64>(32);
    let task = tokio::spawn(async move {
        while let Some(id) = receiver.recv().await {
            if !alive.load(Ordering::Acquire) {
                break;
            }
            let notification = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{{\"requestId\":{id}}}}}\n");
            let sent = tokio::time::timeout(Duration::from_secs(5), async {
                let mut writer = writer.lock().await;
                writer.write_all(notification.as_bytes()).await?;
                writer.flush().await
            })
            .await;
            if !matches!(sent, Ok(Ok(()))) {
                alive.store(false, Ordering::Release);
                if let Some(owner) = &owner {
                    owner.terminate();
                }
                break;
            }
        }
    });
    (sender, task)
}
