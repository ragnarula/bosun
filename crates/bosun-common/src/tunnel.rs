use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

/// Bytes of unacknowledged data a peer may send on one logical connection
/// before the receiver grants more credit. The receiver sends a
/// `WindowUpdate` as its application consumes data, so a slow consumer backs
/// the sender up instead of overflowing a queue.
const INITIAL_WINDOW: u32 = 512 * 1024;
/// Bytes a connection must consume before the receiver grants more credit.
const WINDOW_UPDATE_THRESHOLD: usize = (INITIAL_WINDOW / 2) as usize;
/// Per-connection inbound queue. Sized so a full window of frames always fits
/// no matter how small the frames are.
const RECEIVE_CAPACITY: usize = INITIAL_WINDOW as usize;
/// Bounded queue of frames waiting to be written to the socket.
const WRITER_CAPACITY: usize = 64;
/// Largest allowed frame payload, in bytes. Any larger frame is a protocol
/// error and tears the tunnel down.
const MAX_FRAME_LEN: u32 = 64 * 1024;

const HEADER_LEN: usize = 1 + 8 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FrameType {
    Open = 0x01,
    Data = 0x02,
    Close = 0x03,
    WindowUpdate = 0x04,
}

enum Frame {
    Open { conn_id: u64 },
    Data { conn_id: u64, bytes: Vec<u8> },
    Close { conn_id: u64 },
    WindowUpdate { conn_id: u64, bytes: u32 },
}

enum TunnelWrite {
    Open { conn_id: u64 },
    Data { conn_id: u64, bytes: Vec<u8> },
    Close { conn_id: u64 },
    WindowUpdate { conn_id: u64, bytes: u32 },
}

/// An inbound event for one logical connection.
pub enum Incoming {
    Data(Vec<u8>),
    Close,
}

/// A logical connection the peer opened. Carried out of the reader task so the
/// node side can relay it.
pub struct OpenEvent {
    pub conn_id: u64,
    pub rx: mpsc::Receiver<Incoming>,
}

/// Send-side state of one logical connection: the peer's remaining credit and
/// the waker of a writer that ran out of it.
struct SendWindow {
    remaining: u64,
    waker: Option<Waker>,
}

impl SendWindow {
    fn new() -> Self {
        Self {
            remaining: INITIAL_WINDOW as u64,
            waker: None,
        }
    }
}

struct TunnelInner {
    writer: mpsc::Sender<TunnelWrite>,
    conns: RwLock<HashMap<u64, mpsc::Sender<Incoming>>>,
    send_windows: RwLock<HashMap<u64, SendWindow>>,
    /// Writers parked on a full shared writer queue, woken as it drains.
    writer_space: Mutex<Vec<Waker>>,
    next_id: AtomicU64,
    dead: AtomicBool,
    death: Notify,
}

/// A multiplexed byte tunnel over one outbound connection. The peer that
/// initiated the connection opens logical connections with [`Tunnel::open`];
/// the other side receives them as [`OpenEvent`]s and relays each with
/// [`Tunnel::attach`]. Logical connections are independent full-duplex
/// streams; closing one does not affect the others.
///
/// Each logical connection is flow-controlled in both directions. A sender
/// may put at most [`INITIAL_WINDOW`] bytes in flight before the receiver
/// grants more credit, so no connection can overrun its queues: a slow
/// consumer pauses the producer rather than losing data.
#[derive(Clone)]
pub struct Tunnel {
    inner: Arc<TunnelInner>,
}

impl TunnelInner {
    fn release(&self, conn_id: u64) {
        self.conns.write().unwrap().remove(&conn_id);
        self.send_windows.write().unwrap().remove(&conn_id);
    }
}

impl Tunnel {
    /// Splits `stream` into reader and writer tasks and returns the tunnel
    /// plus a channel of connections the peer opened.
    pub fn new<S>(stream: S) -> (Tunnel, mpsc::UnboundedReceiver<OpenEvent>)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let (writer_tx, writer_rx) = mpsc::channel(WRITER_CAPACITY);
        let (open_tx, open_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(TunnelInner {
            writer: writer_tx,
            conns: RwLock::new(HashMap::new()),
            send_windows: RwLock::new(HashMap::new()),
            writer_space: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            dead: AtomicBool::new(false),
            death: Notify::new(),
        });
        tokio::spawn(writer_loop(write, writer_rx, inner.clone()));
        tokio::spawn(reader_loop(read, inner.clone(), open_tx));
        (Tunnel { inner }, open_rx)
    }

    /// Allocates a connection id, tells the peer to open a connection, and
    /// returns the local end of it. Returns `None` when the tunnel is dead.
    pub async fn open(&self) -> Option<LogicalStream> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return None;
        }
        let conn_id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(RECEIVE_CAPACITY);
        self.inner.conns.write().unwrap().insert(conn_id, tx);
        self.inner
            .send_windows
            .write()
            .unwrap()
            .insert(conn_id, SendWindow::new());
        if self
            .inner
            .writer
            .send(TunnelWrite::Open { conn_id })
            .await
            .is_err()
        {
            self.inner.release(conn_id);
            return None;
        }
        Some(self.stream_for(conn_id, rx))
    }

    /// Returns the local end of a connection the peer opened. Used by the
    /// node side when it receives an [`OpenEvent`].
    pub fn attach(&self, conn_id: u64, rx: mpsc::Receiver<Incoming>) -> Option<LogicalStream> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return None;
        }
        self.inner
            .send_windows
            .write()
            .unwrap()
            .insert(conn_id, SendWindow::new());
        Some(self.stream_for(conn_id, rx))
    }

    /// Resolves once the tunnel has failed and will accept no more
    /// connections. Idempotent: every caller is released at once.
    pub async fn closed(&self) {
        self.inner.death.notified().await;
    }

    fn stream_for(&self, conn_id: u64, rx: mpsc::Receiver<Incoming>) -> LogicalStream {
        LogicalStream {
            conn_id,
            writer: self.inner.writer.clone(),
            inner: self.inner.clone(),
            rx,
            closed: false,
            consumed: 0,
            pending_read: None,
        }
    }
}

/// One full-duplex stream inside a [`Tunnel`]. Reads come from the peer's
/// data frames for this connection; writes go out as data frames. Dropping
/// the stream, or closing it, tells the peer the connection ended.
pub struct LogicalStream {
    conn_id: u64,
    writer: mpsc::Sender<TunnelWrite>,
    inner: Arc<TunnelInner>,
    rx: mpsc::Receiver<Incoming>,
    closed: bool,
    /// Bytes delivered to the application since the last `WindowUpdate`.
    consumed: usize,
    /// Remainder of a data frame too large for the read buffer.
    pending_read: Option<Vec<u8>>,
}

impl LogicalStream {
    fn consume(&mut self, n: usize) {
        self.consumed += n;
        if self.consumed >= WINDOW_UPDATE_THRESHOLD {
            let bytes = self.consumed as u32;
            self.consumed = 0;
            send_window_update(&self.inner, self.conn_id, bytes);
        }
    }
}

impl AsyncRead for LogicalStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if let Some(mut pending) = self.pending_read.take() {
                let n = pending.len().min(buf.remaining());
                buf.put_slice(&pending[..n]);
                self.consume(n);
                if n < pending.len() {
                    pending.drain(..n);
                    self.pending_read = Some(pending);
                }
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Incoming::Data(bytes))) if bytes.is_empty() => continue,
                Poll::Ready(Some(Incoming::Data(bytes))) => {
                    if bytes.len() > buf.remaining() {
                        let n = buf.remaining();
                        buf.put_slice(&bytes[..n]);
                        self.consume(n);
                        self.pending_read = Some(bytes[n..].to_vec());
                    } else {
                        let n = bytes.len();
                        buf.put_slice(&bytes);
                        self.consume(n);
                    }
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(Incoming::Close)) | Poll::Ready(None) => {
                    self.closed = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for LogicalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "logical connection is closed",
            )));
        }
        if self.inner.dead.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel is closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if buf.len() as u32 > MAX_FRAME_LEN {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write exceeds the maximum frame size",
            )));
        }

        // Take credit before queuing the frame. The peer replenishes it with
        // WindowUpdate frames as it consumes, so a slow peer parks this write
        // instead of overflowing the tunnel.
        let n = {
            let mut windows = self.inner.send_windows.write().unwrap();
            let Some(window) = windows.get_mut(&self.conn_id) else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "logical connection is closed",
                )));
            };
            if window.remaining == 0 {
                window.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let n = buf.len().min(window.remaining as usize);
            window.remaining -= n as u64;
            n
        };

        let bytes = buf[..n].to_vec();
        match self.writer.try_send(TunnelWrite::Data {
            conn_id: self.conn_id,
            bytes,
        }) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // The frame was not queued; give the credit back and wait for
                // the writer loop to drain the shared queue.
                if let Some(window) = self
                    .inner
                    .send_windows
                    .write()
                    .unwrap()
                    .get_mut(&self.conn_id)
                {
                    window.remaining += n as u64;
                }
                self.inner
                    .writer_space
                    .lock()
                    .unwrap()
                    .push(cx.waker().clone());
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel write queue is closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.closed = true;
        send_close(&self.inner, self.conn_id);
        self.inner.release(self.conn_id);
        Poll::Ready(Ok(()))
    }
}

impl Drop for LogicalStream {
    fn drop(&mut self) {
        if !self.closed {
            send_close(&self.inner, self.conn_id);
        }
        self.inner.release(self.conn_id);
    }
}

/// Queues a close frame, delivering it asynchronously when the writer queue
/// is full so the peer never waits forever on a connection that is gone.
fn send_close(inner: &Arc<TunnelInner>, conn_id: u64) {
    let frame = TunnelWrite::Close { conn_id };
    if inner.writer.try_send(frame).is_ok() {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let inner = inner.clone();
    tokio::spawn(async move {
        let _ = inner.writer.send(TunnelWrite::Close { conn_id }).await;
    });
}

/// Grants the peer more credit for one connection.
fn send_window_update(inner: &Arc<TunnelInner>, conn_id: u64, bytes: u32) {
    let frame = TunnelWrite::WindowUpdate { conn_id, bytes };
    if inner.writer.try_send(frame).is_ok() {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let inner = inner.clone();
    tokio::spawn(async move {
        let _ = inner
            .writer
            .send(TunnelWrite::WindowUpdate { conn_id, bytes })
            .await;
    });
}

async fn writer_loop<W>(mut write: W, mut rx: mpsc::Receiver<TunnelWrite>, inner: Arc<TunnelInner>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(item) = rx.recv().await {
        let frame = match item {
            TunnelWrite::Open { conn_id } => Frame::Open { conn_id },
            TunnelWrite::Data { conn_id, bytes } => Frame::Data { conn_id, bytes },
            TunnelWrite::Close { conn_id } => Frame::Close { conn_id },
            TunnelWrite::WindowUpdate { conn_id, bytes } => Frame::WindowUpdate { conn_id, bytes },
        };
        if let Err(error) = write_frame(&mut write, &frame).await {
            warn!(error = %error, "tunnel writer failed; closing tunnel");
            mark_dead(&inner);
            return;
        }
        wake_blocked_writers(&inner);
    }
    mark_dead(&inner);
}

/// Wakes writers parked on a full shared writer queue. Called whenever the
/// writer loop drains a frame and frees a slot.
fn wake_blocked_writers(inner: &Arc<TunnelInner>) {
    let wakers = std::mem::take(&mut *inner.writer_space.lock().unwrap());
    for waker in wakers {
        waker.wake();
    }
}

async fn reader_loop<R>(
    mut read: R,
    inner: Arc<TunnelInner>,
    open_tx: mpsc::UnboundedSender<OpenEvent>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        let frame = match read_frame(&mut read).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                debug!(error = %error, "tunnel reader failed; closing tunnel");
                break;
            }
        };
        match frame {
            Frame::Open { conn_id } => {
                let (tx, rx) = mpsc::channel(RECEIVE_CAPACITY);
                inner.conns.write().unwrap().insert(conn_id, tx);
                let _ = open_tx.send(OpenEvent { conn_id, rx });
            }
            Frame::Data { conn_id, bytes } => {
                let conns = inner.conns.read().unwrap();
                let Some(tx) = conns.get(&conn_id) else {
                    continue;
                };
                match tx.try_send(Incoming::Data(bytes)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => continue,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        drop(conns);
                        warn!(
                            conn_id,
                            "logical connection receive queue overflowed despite flow control; closing tunnel"
                        );
                        mark_dead(&inner);
                        return;
                    }
                }
            }
            Frame::Close { conn_id } => {
                if let Some(tx) = inner.conns.write().unwrap().remove(&conn_id) {
                    let _ = tx.try_send(Incoming::Close);
                }
                if let Some(window) = inner.send_windows.write().unwrap().remove(&conn_id)
                    && let Some(waker) = window.waker
                {
                    waker.wake();
                }
            }
            Frame::WindowUpdate { conn_id, bytes } => {
                let mut windows = inner.send_windows.write().unwrap();
                if let Some(window) = windows.get_mut(&conn_id) {
                    window.remaining += bytes as u64;
                    if let Some(waker) = window.waker.take() {
                        waker.wake();
                    }
                }
            }
        }
    }
    mark_dead(&inner);
}

fn mark_dead(inner: &Arc<TunnelInner>) {
    if inner.dead.swap(true, Ordering::SeqCst) {
        return;
    }
    let conns = inner.conns.read().unwrap();
    for tx in conns.values() {
        let _ = tx.try_send(Incoming::Close);
    }
    drop(conns);
    let writer_wakers = std::mem::take(&mut *inner.writer_space.lock().unwrap());
    for waker in writer_wakers {
        waker.wake();
    }
    for window in inner.send_windows.write().unwrap().values_mut() {
        if let Some(waker) = window.waker.take() {
            waker.wake();
        }
    }
    inner.death.notify_waiters();
}

async fn write_frame<W>(write: &mut W, frame: &Frame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match frame {
        Frame::WindowUpdate { conn_id, bytes } => {
            let mut header = [0u8; HEADER_LEN];
            header[0] = FrameType::WindowUpdate as u8;
            header[1..9].copy_from_slice(&conn_id.to_le_bytes());
            header[9..HEADER_LEN].copy_from_slice(&4u32.to_le_bytes());
            write.write_all(&header).await?;
            write.write_all(&bytes.to_le_bytes()).await
        }
        Frame::Open { conn_id } => {
            let mut header = [0u8; HEADER_LEN];
            header[0] = FrameType::Open as u8;
            header[1..9].copy_from_slice(&conn_id.to_le_bytes());
            write.write_all(&header).await
        }
        Frame::Data { conn_id, bytes } => {
            let mut header = [0u8; HEADER_LEN];
            header[0] = FrameType::Data as u8;
            header[1..9].copy_from_slice(&conn_id.to_le_bytes());
            header[9..HEADER_LEN].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            write.write_all(&header).await?;
            write.write_all(bytes).await
        }
        Frame::Close { conn_id } => {
            let mut header = [0u8; HEADER_LEN];
            header[0] = FrameType::Close as u8;
            header[1..9].copy_from_slice(&conn_id.to_le_bytes());
            write.write_all(&header).await
        }
    }
}

async fn read_frame<R>(read: &mut R) -> io::Result<Option<Frame>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; HEADER_LEN];
    let mut read_count = 0;
    while read_count < HEADER_LEN {
        match read.read(&mut header[read_count..]).await {
            Ok(0) => break,
            Ok(n) => read_count += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if read_count == 0 {
        return Ok(None);
    }
    if read_count < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated frame header",
        ));
    }
    let frame_type = header[0];
    let conn_id = u64::from_le_bytes(header[1..9].try_into().expect("fixed header slice"));
    let len = u32::from_le_bytes(
        header[9..HEADER_LEN]
            .try_into()
            .expect("fixed header slice"),
    );
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the maximum size",
        ));
    }
    if frame_type == FrameType::WindowUpdate as u8 && len != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "window update frame must carry a 4-byte credit",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    read.read_exact(&mut payload).await?;
    let frame = match frame_type {
        _ if frame_type == FrameType::Open as u8 => Frame::Open { conn_id },
        _ if frame_type == FrameType::Data as u8 => Frame::Data {
            conn_id,
            bytes: payload,
        },
        _ if frame_type == FrameType::Close as u8 => Frame::Close { conn_id },
        _ if frame_type == FrameType::WindowUpdate as u8 => Frame::WindowUpdate {
            conn_id,
            bytes: u32::from_le_bytes(payload[..4].try_into().expect("fixed credit slice")),
        },
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame type {other}"),
            ));
        }
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    fn pair() -> (Tunnel, Tunnel, mpsc::UnboundedReceiver<OpenEvent>) {
        let (cp_side, node_side) = duplex(1024 * 1024);
        let (cp_tunnel, _opens) = Tunnel::new(cp_side);
        let (node_tunnel, opens) = Tunnel::new(node_side);
        (cp_tunnel, node_tunnel, opens)
    }

    async fn attach(event: OpenEvent, tunnel: &Tunnel) -> LogicalStream {
        tunnel.attach(event.conn_id, event.rx).expect("attach")
    }

    #[tokio::test]
    async fn streams_bytes_both_ways() {
        let (cp, node, mut opens) = pair();
        let mut client = cp.open().await.expect("open");
        let mut server = attach(opens.recv().await.expect("open event"), &node).await;

        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        server.write_all(b"world").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    #[tokio::test]
    async fn concurrent_connections_are_isolated() {
        let (cp, node, mut opens) = pair();
        let mut first = cp.open().await.expect("open");
        let mut second = cp.open().await.expect("open");

        let first_event = opens.recv().await.expect("first open event");
        let second_event = opens.recv().await.expect("second open event");
        let mut first_server = attach(first_event, &node).await;
        let mut second_server = attach(second_event, &node).await;

        first.write_all(b"one").await.unwrap();
        second.write_all(b"two").await.unwrap();

        let mut buf = [0u8; 3];
        first_server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"one");
        let mut buf = [0u8; 3];
        second_server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"two");
    }

    #[tokio::test]
    async fn dropping_the_client_end_closes_the_server_end() {
        let (cp, node, mut opens) = pair();
        let client = cp.open().await.expect("open");
        let mut server = attach(opens.recv().await.expect("open event"), &node).await;

        drop(client);

        let mut buf = [0u8; 4];
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "server read should hit EOF");
    }

    #[tokio::test]
    async fn data_published_after_attach_is_delivered() {
        let (cp, node, mut opens) = pair();
        let mut client = cp.open().await.expect("open");
        let event = opens.recv().await.expect("open event");

        client.write_all(b"early").await.unwrap();
        let mut server = attach(event, &node).await;

        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"early");
    }

    #[tokio::test]
    async fn open_fails_once_the_tunnel_is_dead() {
        let (a, b) = duplex(1024);
        let (cp, _opens) = Tunnel::new(a);
        drop(b);

        // The reader observes EOF and marks the tunnel dead.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while cp.open().await.is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(cp.open().await.is_none());
    }

    /// A producer that outruns its consumer must pause, never lose data, and
    /// finish once the consumer catches up. Exercises the flow-control window
    /// across the whole tunnel.
    #[tokio::test]
    async fn a_slow_consumer_never_loses_data() {
        let (a, b) = duplex(64 * 1024);
        let (cp, _opens) = Tunnel::new(a);
        let (node, mut opens) = Tunnel::new(b);
        let mut client = cp.open().await.expect("open");
        let event = opens.recv().await.expect("open event");
        let mut server = node.attach(event.conn_id, event.rx).expect("attach");

        let total = INITIAL_WINDOW as usize * 4;
        let chunk = vec![b'x'; 8192];
        let writer = tokio::spawn(async move {
            let mut written = 0;
            while written < total {
                let n = client.write(&chunk).await.unwrap();
                assert_ne!(n, 0, "writer made no progress while blocked");
                written += n;
            }
            client.shutdown().await.unwrap();
        });

        let mut received = 0;
        let mut buf = vec![0u8; 8192];
        while received < total {
            let n = server.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "connection closed before all data arrived");
            received += n;
            tokio::task::yield_now().await;
        }
        assert_eq!(received, total);
        writer.await.unwrap();
    }
}
