//! Bounded, nonblocking admission to dedicated OS writer threads. A stalled
//! pipe must never occupy the task's executor or Tokio's blocking pool (whose
//! shutdown waits for outstanding work). Progress is best effort; JSON delivery
//! is acknowledged and has its own one-second deadline.

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use instagent::agent::task::RunReport;
use tokio::sync::{mpsc, oneshot};

const QUEUE_BYTES: usize = 1024 * 1024;
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(1);

enum Job {
    Bytes(Vec<u8>),
    Flush(oneshot::Sender<io::Result<()>>),
    Report(RunReport, oneshot::Sender<io::Result<()>>),
}

#[derive(Clone)]
pub struct QueuedWriter {
    tx: mpsc::Sender<Job>,
    pending: Arc<AtomicUsize>,
}

impl QueuedWriter {
    fn new(mut writer: impl Write + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::channel(256);
        let pending = Arc::new(AtomicUsize::new(0));
        let accounting = pending.clone();
        std::thread::spawn(move || {
            let mut failure: Option<(io::ErrorKind, String)> = None;
            while let Some(job) = rx.blocking_recv() {
                let prior_error = || match &failure {
                    Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
                    None => Ok(()),
                };
                let (result, reply) = match job {
                    Job::Bytes(bytes) => {
                        let result = prior_error()
                            .and_then(|()| writer.write_all(&bytes))
                            .and_then(|()| writer.flush());
                        accounting.fetch_sub(bytes.len(), Ordering::Relaxed);
                        (result, None)
                    }
                    Job::Flush(reply) => (prior_error().and_then(|()| writer.flush()), Some(reply)),
                    Job::Report(report, reply) => {
                        let result = prior_error().and_then(|()| {
                            serde_json::to_writer(&mut writer, &report)
                                .map_err(io::Error::other)?;
                            writeln!(writer)?;
                            writer.flush()
                        });
                        (result, Some(reply))
                    }
                };
                if let Err(err) = &result {
                    failure = Some((err.kind(), err.to_string()));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
        });
        Self { tx, pending }
    }

    async fn acknowledge(&self, report: Option<RunReport>) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        let job = match report {
            Some(report) => Job::Report(report, tx),
            None => Job::Flush(tx),
        };
        self.tx
            .send(job)
            .await
            .map_err(|_| io::Error::other("output writer stopped"))?;
        rx.await
            .map_err(|_| io::Error::other("output writer stopped"))?
    }

    pub async fn finish(&self, budget: Duration) -> io::Result<()> {
        tokio::time::timeout(budget, self.acknowledge(None))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "output delivery timed out"))?
    }

    pub async fn report(&self, report: RunReport) -> io::Result<()> {
        tokio::time::timeout(DELIVERY_TIMEOUT, self.acknowledge(Some(report)))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "JSON result delivery timed out")
            })?
    }
}

impl Write for QueuedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                pending
                    .checked_add(bytes.len())
                    .filter(|total| *total <= QUEUE_BYTES)
            })
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        if self.tx.try_send(Job::Bytes(bytes.to_vec())).is_err() {
            self.pending.fetch_sub(bytes.len(), Ordering::Relaxed);
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn stdout() -> QueuedWriter {
    static WRITER: OnceLock<QueuedWriter> = OnceLock::new();
    WRITER
        .get_or_init(|| QueuedWriter::new(std::io::stdout()))
        .clone()
}

pub fn stderr() -> QueuedWriter {
    static WRITER: OnceLock<QueuedWriter> = OnceLock::new();
    WRITER
        .get_or_init(|| QueuedWriter::new(std::io::stderr()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gate {
        entered: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn stalled_writer_has_bounded_memory_and_a_cancellable_flush() {
        let (entered, started) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let mut writer = QueuedWriter::new(Gate {
            entered: Some(entered),
            release: gate,
        });
        writer.write_all(b"x").unwrap();
        started.await.unwrap();
        let chunk = vec![b'x'; 8192];
        let mut admitted = 1;
        while writer.write_all(&chunk).is_ok() {
            admitted += chunk.len();
        }
        assert!(admitted <= QUEUE_BYTES);
        assert_eq!(writer.pending.load(Ordering::Relaxed), admitted);
        let error = writer.finish(Duration::from_millis(10)).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        release.send(()).unwrap();
        writer.finish(Duration::from_secs(2)).await.unwrap();
        assert_eq!(writer.pending.load(Ordering::Relaxed), 0);
    }
}
