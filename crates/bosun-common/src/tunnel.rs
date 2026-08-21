use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

/// Bounded per-connection queue of incoming data frames.
const CHANNEL_CAPACITY: usize = 32;
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
}

enum Frame {
    Open { conn_id: u64 },
    Data { conn_id: u64, bytes: Vec<u8> },
    Close { conn_id: u64 },
}

enum TunnelWrite {
    Open { conn_id: u64 },
    Data { conn_id: u64, bytes: Vec<u8> },
    Close { conn_id: u64 },
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

struct TunnelInner {
    writer: mpsc::Sender<TunnelWrite>,
    conns: RwLock<HashMap<u64, mpsc::Sender<Incoming>>>,
    next_id: AtomicU64,
    dead: AtomicBool,
    death: Notify,
}

/// A multiplexed byte tunnel over one outbound connection. The peer that
/// initiated the connection opens logical connections with [`Tunnel::open`];
/// the other side receives them as [`OpenEvent`]s and relays each with
/// [`Tunnel::attach`]. Logical connections are independent full-duplex
/// streams; closing one does not affect the others.
#[derive(Clone)]
pub struct Tunnel {
    inner: Arc<TunnelInner>,
}

impl TunnelInner {
    fn release(&self, conn_id: u64) {
        self.conns.write().unwrap().remove(&conn_id);
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
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        self.inner.conns.write().unwrap().insert(conn_id, tx);
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
}

impl AsyncRead for LogicalStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(Incoming::Data(bytes))) if bytes.is_empty() => continue,
                Poll::Ready(Some(Incoming::Data(bytes))) => {
                    buf.put_slice(&bytes);
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
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "logical connection is closed",
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
        // Bounded by the writer channel capacity. A full queue means the peer
        // is not reading; the connection is failing anyway.
        let bytes = buf.to_vec();
        match self.writer.try_send(TunnelWrite::Data {
            conn_id: self.conn_id,
            bytes,
        }) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel write queue is full or closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.closed = true;
        let _ = self.writer.try_send(TunnelWrite::Close {
            conn_id: self.conn_id,
        });
        self.inner.release(self.conn_id);
        Poll::Ready(Ok(()))
    }
}

impl Drop for LogicalStream {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.writer.try_send(TunnelWrite::Close {
                conn_id: self.conn_id,
            });
        }
        self.inner.release(self.conn_id);
    }
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
        };
        if let Err(error) = write_frame(&mut write, &frame).await {
            warn!(error = %error, "tunnel writer failed; closing tunnel");
            mark_dead(&inner);
            return;
        }
    }
    mark_dead(&inner);
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
                let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
                inner.conns.write().unwrap().insert(conn_id, tx);
                let _ = open_tx.send(OpenEvent { conn_id, rx });
            }
            Frame::Data { conn_id, bytes } => {
                let conns = inner.conns.read().unwrap();
                let Some(tx) = conns.get(&conn_id) else {
                    continue;
                };
                if tx.try_send(Incoming::Data(bytes)).is_err() {
                    drop(conns);
                    debug!(
                        conn_id,
                        "logical connection overflowed its buffer; closing it"
                    );
                    let _ = inner.writer.try_send(TunnelWrite::Close { conn_id });
                    inner.conns.write().unwrap().remove(&conn_id);
                }
            }
            Frame::Close { conn_id } => {
                if let Some(tx) = inner.conns.write().unwrap().remove(&conn_id) {
                    let _ = tx.try_send(Incoming::Close);
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
    inner.death.notify_waiters();
}

async fn write_frame<W>(write: &mut W, frame: &Frame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let (frame_type, conn_id, payload) = match frame {
        Frame::Open { conn_id } => (FrameType::Open as u8, *conn_id, &[][..]),
        Frame::Data { conn_id, bytes } => (FrameType::Data as u8, *conn_id, bytes.as_slice()),
        Frame::Close { conn_id } => (FrameType::Close as u8, *conn_id, &[][..]),
    };
    let mut header = [0u8; HEADER_LEN];
    header[0] = frame_type;
    header[1..9].copy_from_slice(&conn_id.to_le_bytes());
    header[9..HEADER_LEN].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    write.write_all(&header).await?;
    if !payload.is_empty() {
        write.write_all(payload).await?;
    }
    Ok(())
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
    let mut payload = vec![0u8; len as usize];
    read.read_exact(&mut payload).await?;
    let frame = match frame_type {
        _ if frame_type == FrameType::Open as u8 => Frame::Open { conn_id },
        _ if frame_type == FrameType::Data as u8 => Frame::Data {
            conn_id,
            bytes: payload,
        },
        _ if frame_type == FrameType::Close as u8 => Frame::Close { conn_id },
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
}
