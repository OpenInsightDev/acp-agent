use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{Router, serve::Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const FORCE_CLOSE_GRACE: Duration = Duration::from_secs(1);

pub(crate) async fn serve_listener(
    listener: TcpListener,
    router: Router,
    cancel: watch::Sender<bool>,
) -> Result<()> {
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(await_termination_signal(shutdown));
    serve_with_shutdown(listener, router, shutdown_rx, cancel, SHUTDOWN_GRACE).await
}

/// Feeds a shutdown watch channel when the process receives SIGINT or SIGTERM.
///
/// The sender is retained for the task's lifetime: dropping it would make the
/// shutdown receiver treat the watch as closed and stop the server without a
/// signal ever arriving.
pub(crate) async fn await_termination_signal(shutdown: watch::Sender<bool>) {
    match wait_for_termination().await {
        Ok(()) => {
            shutdown.send_replace(true);
        }
        Err(error) => eprintln!("failed to install termination signal handler: {error}"),
    }
    std::future::pending::<()>().await;
}

async fn wait_for_termination() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

/// Serves `router` on `listener` until `shutdown_rx` is signaled, then drains
/// active connections within `shutdown_grace` before cancelling the rest.
///
/// Shared by the standalone `serve` command (signal-driven) and named servers
/// (control-plane driven); `cancel` is the sender wired into every agent.
pub(crate) async fn serve_with_shutdown(
    listener: TcpListener,
    router: Router,
    shutdown_rx: watch::Receiver<bool>,
    cancel: watch::Sender<bool>,
    shutdown_grace: Duration,
) -> Result<()> {
    let (force_close, force_close_rx) = watch::channel(false);
    let listener = ForceCloseListener {
        inner: listener,
        force_close: force_close_rx,
    };
    let mut server_shutdown = shutdown_rx.clone();
    let mut supervisor_shutdown = shutdown_rx;
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_shutdown(&mut server_shutdown).await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("ACP HTTP server failed"),
        () = wait_for_shutdown(&mut supervisor_shutdown) => {
            match timeout(shutdown_grace, &mut server).await {
                Ok(result) => result.context("ACP HTTP server failed"),
                Err(_) => {
                    // Cancel connections that did not drain within the grace period.
                    cancel.send_replace(true);
                    force_close.send_replace(true);
                    timeout(FORCE_CLOSE_GRACE, &mut server)
                        .await
                        .context("ACP HTTP connections did not close after forced shutdown")??;
                    Ok(())
                },
            }
        }
    }
}

/// Waits until `receiver` observes `true` or its sender is dropped.
pub(crate) async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

/// TCP listener that aborts every accepted connection once the shutdown drain
/// grace expired, so connections that never observed the cancellation still
/// close instead of blocking shutdown indefinitely.
struct ForceCloseListener {
    inner: TcpListener,
    force_close: watch::Receiver<bool>,
}

impl Listener for ForceCloseListener {
    type Io = ForceCloseIo;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    return (ForceCloseIo::new(stream, self.force_close.clone()), address);
                }
                Err(error) => {
                    eprintln!("failed to accept ACP connection: {error}");
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Connection I/O that starts failing once the shutdown drain grace expired.
struct ForceCloseIo {
    inner: tokio::net::TcpStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl ForceCloseIo {
    fn new(inner: tokio::net::TcpStream, mut force_close: watch::Receiver<bool>) -> Self {
        Self {
            inner,
            cancelled: Box::pin(async move {
                wait_for_shutdown(&mut force_close).await;
            }),
        }
    }

    fn poll_cancelled(&mut self, context: &mut TaskContext<'_>) -> std::io::Result<()> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "ACP shutdown grace expired",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for ForceCloseIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for ForceCloseIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.poll_cancelled(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}
