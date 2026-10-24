//! A minimal ZMTP 3.0 client (<https://rfc.zeromq.org/spec/23/>).
//!
//! Only what a Jupyter frontend needs: TCP transport, the NULL security
//! mechanism, and the client side of three socket roles: DEALER (shell,
//! control, stdin, heartbeat) and SUB (iopub). There is no HWM, no
//! routing, no reconnection: each connection is a single peer, and the kernel
//! actor reconnects by building a new connection for a new kernel process.
//!
//! We announce ZMTP 3.0 (not 3.1) so that libzmq peers use the 3.0 rules:
//! subscriptions are plain messages prefixed with `0x01` and the peer never
//! sends PING commands unless explicitly configured (we answer them anyway).

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use smol::Timer;
use smol::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use smol::net::TcpStream;

const FLAG_MORE: u8 = 0x01;
const FLAG_LONG: u8 = 0x02;
const FLAG_COMMAND: u8 = 0x04;

/// Sanity cap on a single frame. Jupyter outputs can be large (images,
/// dataframes), but nothing legitimate approaches this.
const MAX_FRAME: u64 = 2 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketType {
    Dealer,
    Sub,
}

impl SocketType {
    fn as_str(self) -> &'static str {
        match self {
            SocketType::Dealer => "DEALER",
            SocketType::Sub => "SUB",
        }
    }

    fn accepts(self, peer: &[u8]) -> bool {
        match self {
            SocketType::Dealer => matches!(peer, b"ROUTER" | b"REP" | b"DEALER"),
            SocketType::Sub => matches!(peer, b"PUB" | b"XPUB"),
        }
    }
}

/// The 64-byte greeting: signature, version 3.0, mechanism NULL, as-server 0.
fn greeting() -> [u8; 64] {
    let mut g = [0u8; 64];
    g[0] = 0xFF;
    // Padding: libzmq encodes "identity length + 1" here for ZMTP 1.0
    // compatibility. Peers ignore it.
    g[8] = 0x01;
    g[9] = 0x7F;
    g[10] = 3;
    g[11] = 0;
    g[12..16].copy_from_slice(b"NULL");
    g
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn push_frame_header(out: &mut Vec<u8>, flags: u8, len: usize) {
    if len > 255 {
        out.push(flags | FLAG_LONG);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    } else {
        out.push(flags);
        out.push(len as u8);
    }
}

fn command(name: &str, body: &[u8]) -> Vec<u8> {
    let len = 1 + name.len() + body.len();
    let mut out = Vec::with_capacity(len + 9);
    push_frame_header(&mut out, FLAG_COMMAND, len);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(body);
    out
}

fn push_property(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

fn ready_command(socket_type: SocketType, identity: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    push_property(&mut body, "Socket-Type", socket_type.as_str().as_bytes());
    if let Some(identity) = identity {
        push_property(&mut body, "Identity", identity);
    }
    command("READY", &body)
}

/// Find a property in a READY command body.
fn property<'a>(mut body: &'a [u8], wanted: &str) -> Option<&'a [u8]> {
    while !body.is_empty() {
        let name_len = *body.first()? as usize;
        let name = body.get(1..1 + name_len)?;
        let rest = body.get(1 + name_len..)?;
        let value_len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let value = rest.get(4..4 + value_len)?;
        if name.eq_ignore_ascii_case(wanted.as_bytes()) {
            return Some(value);
        }
        body = &rest[4 + value_len..];
    }
    None
}

/// Append one multipart message to `out` in wire format.
pub(crate) fn encode_message(out: &mut Vec<u8>, frames: &[&[u8]]) {
    let total: usize = frames.iter().map(|f| f.len() + 9).sum();
    out.reserve(total);
    for (i, frame) in frames.iter().enumerate() {
        let flags = if i + 1 < frames.len() { FLAG_MORE } else { 0 };
        push_frame_header(out, flags, frame.len());
        out.extend_from_slice(frame);
    }
}

/// Wire bytes for a SUB subscription to every topic (ZMTP 3.0 style).
pub(crate) fn subscribe_all() -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    encode_message(&mut out, &[&[0x01]]);
    out
}

pub(crate) enum Incoming {
    Message(Vec<Vec<u8>>),
    Command { name: Vec<u8>, body: Vec<u8> },
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head).await?;
    let flags = head[0];
    let len = if flags & FLAG_LONG != 0 {
        let mut rest = [0u8; 7];
        r.read_exact(&mut rest).await?;
        let mut size = [0u8; 8];
        size[0] = head[1];
        size[1..].copy_from_slice(&rest);
        u64::from_be_bytes(size)
    } else {
        head[1] as u64
    };
    if len > MAX_FRAME {
        return Err(invalid(format!("ZMTP frame too large ({len} bytes)")));
    }
    if len <= 64 * 1024 {
        let mut body = vec![0u8; len as usize];
        r.read_exact(&mut body).await?;
        return Ok((flags, body));
    }
    // Large frame: grow as bytes actually arrive instead of trusting the
    // declared size with one huge allocation.
    let mut body = Vec::with_capacity(1 << 20);
    (&mut *r).take(len).read_to_end(&mut body).await?;
    if body.len() as u64 != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated ZMTP frame",
        ));
    }
    Ok((flags, body))
}

/// Read one complete message (all frames) or one command.
pub(crate) async fn read<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Incoming> {
    let mut frames = Vec::new();
    loop {
        let (flags, body) = read_frame(r).await?;
        if flags & FLAG_COMMAND != 0 {
            if !frames.is_empty() {
                return Err(invalid("ZMTP command inside a multipart message"));
            }
            let name_len = *body.first().ok_or_else(|| invalid("empty ZMTP command"))? as usize;
            if body.len() < 1 + name_len {
                return Err(invalid("truncated ZMTP command"));
            }
            let name = body[1..1 + name_len].to_vec();
            let body = body[1 + name_len..].to_vec();
            return Ok(Incoming::Command { name, body });
        }
        frames.push(body);
        if flags & FLAG_MORE == 0 {
            return Ok(Incoming::Message(frames));
        }
    }
}

/// Reply to a PING command; `None` for commands we ignore.
pub(crate) fn answer_command(name: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    if name == b"PING" {
        // PING body: 2-byte TTL then up to 16 bytes of context to echo.
        let context = body.get(2..).unwrap_or(&[]);
        return Some(command("PONG", &context[..context.len().min(16)]));
    }
    None
}

/// A connected, handshaken ZMTP connection, split for concurrent use.
pub(crate) struct Conn {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
}

/// Greeting + NULL handshake on an established TCP stream.
pub(crate) async fn handshake(
    stream: TcpStream,
    socket_type: SocketType,
    identity: Option<&[u8]>,
) -> io::Result<Conn> {
    stream.set_nodelay(true)?;
    let mut writer = stream.clone();
    let mut raw = stream.clone();
    writer.write_all(&greeting()).await?;

    let mut peer = [0u8; 64];
    raw.read_exact(&mut peer).await?;
    if peer[0] != 0xFF || peer[9] & 0x01 == 0 {
        return Err(invalid("peer is not a ZMTP 2+ endpoint"));
    }
    if peer[10] < 3 {
        return Err(invalid(format!(
            "peer speaks ZMTP {}.x, need 3.x",
            peer[10]
        )));
    }
    let mechanism = &peer[12..32];
    let mechanism = &mechanism[..mechanism.iter().position(|&b| b == 0).unwrap_or(20)];
    if mechanism != b"NULL" {
        return Err(invalid(format!(
            "kernel requires ZMQ security mechanism {:?}; only NULL is supported",
            String::from_utf8_lossy(mechanism)
        )));
    }

    writer
        .write_all(&ready_command(socket_type, identity))
        .await?;

    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    match read(&mut reader).await? {
        Incoming::Command { name, body } if name == b"READY" => {
            if let Some(peer_type) = property(&body, "Socket-Type")
                && !socket_type.accepts(peer_type)
            {
                return Err(invalid(format!(
                    "incompatible peer socket type {}",
                    String::from_utf8_lossy(peer_type)
                )));
            }
        }
        Incoming::Command { name, body } if name == b"ERROR" => {
            let reason = body.get(1..).unwrap_or(&[]);
            return Err(invalid(format!(
                "peer rejected handshake: {}",
                String::from_utf8_lossy(reason)
            )));
        }
        _ => return Err(invalid("expected READY command from peer")),
    }
    Ok(Conn { reader, writer })
}

/// Connect to `addr`, retrying quickly while the kernel has not bound the port
/// yet (connection refused), then handshake. Fails at `deadline`.
pub(crate) async fn connect(
    addr: SocketAddr,
    socket_type: SocketType,
    identity: Option<&[u8]>,
    deadline: Instant,
) -> io::Result<Conn> {
    let mut delay = Duration::from_millis(1);
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                let handshake_deadline = Instant::now() + Duration::from_secs(10);
                return crate::util::with_deadline(
                    handshake(stream, socket_type, identity),
                    handshake_deadline,
                )
                .await
                .unwrap_or_else(|| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ZMTP handshake timed out",
                    ))
                });
            }
            Err(e) if is_retryable(&e) => {}
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("kernel never started listening on {addr}"),
            ));
        }
        Timer::after(delay).await;
        delay = (delay * 2).min(Duration::from_millis(16));
    }
}

fn is_retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_short_and_long() {
        let big = vec![7u8; 300];
        let mut wire = Vec::new();
        encode_message(&mut wire, &[b"<IDS|MSG>", b"", &big]);
        // short frame: flags + 1 byte size; long: flags + 8 bytes size
        assert_eq!(wire[0], FLAG_MORE);
        assert_eq!(wire[1], 9);
        let frames = smol::block_on(async {
            match read(&mut &wire[..]).await.unwrap() {
                Incoming::Message(f) => f,
                Incoming::Command { .. } => panic!("expected message"),
            }
        });
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], b"<IDS|MSG>");
        assert!(frames[1].is_empty());
        assert_eq!(frames[2], big);
    }

    #[test]
    fn large_and_truncated_frames() {
        let big: Vec<u8> = (0..3_000_000u32).map(|i| i as u8).collect();
        let mut wire = Vec::new();
        encode_message(&mut wire, &[&big]);
        let frames = smol::block_on(async {
            match read(&mut &wire[..]).await.unwrap() {
                Incoming::Message(f) => f,
                Incoming::Command { .. } => panic!("expected message"),
            }
        });
        assert_eq!(frames, vec![big]);
        let cut = &wire[..wire.len() - 1];
        let err = smol::block_on(async { read(&mut &cut[..]).await.err().unwrap() });
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // A lying size header must not allocate gigabytes up front.
        let liar = [FLAG_LONG, 0, 0, 0, 0x7F, 0, 0, 0, 0, 1, 2, 3];
        assert!(smol::block_on(async { read(&mut &liar[..]).await }).is_err());
    }

    #[test]
    fn ready_command_carries_properties() {
        let cmd = ready_command(SocketType::Dealer, Some(b"abc"));
        let parsed = smol::block_on(async { read(&mut &cmd[..]).await.unwrap() });
        let Incoming::Command { name, body } = parsed else {
            panic!("expected command")
        };
        assert_eq!(name, b"READY");
        assert_eq!(property(&body, "Socket-Type"), Some(&b"DEALER"[..]));
        assert_eq!(property(&body, "identity"), Some(&b"abc"[..]));
        assert_eq!(property(&body, "Missing"), None);
    }

    #[test]
    fn ping_gets_pong_with_context() {
        let pong = answer_command(b"PING", &[0, 10, 1, 2, 3]).unwrap();
        let parsed = smol::block_on(async { read(&mut &pong[..]).await.unwrap() });
        let Incoming::Command { name, body } = parsed else {
            panic!()
        };
        assert_eq!(name, b"PONG");
        assert_eq!(body, vec![1, 2, 3]);
    }

    #[test]
    fn greeting_layout() {
        let g = greeting();
        assert_eq!(g.len(), 64);
        assert_eq!((g[0], g[9], g[10], g[11]), (0xFF, 0x7F, 3, 0));
        assert_eq!(&g[12..16], b"NULL");
        assert_eq!(g[32], 0);
    }
}
