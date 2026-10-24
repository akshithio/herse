//! End-to-end encryption of the session channel.
//!
//! # Handshake (WebSocket binary messages)
//!
//! ```text
//! guest -> host   "HERSE1" 0x01 version  client_nonce[32]          (plaintext)
//! host  -> guest  "HERSE1" 0x02 version  server_nonce[32]          (plaintext)
//! guest -> host   frame #0: Auth   (sealed with the c2s key)
//! host  -> guest  frame #0: Welcome {role} (sealed with the s2c key)
//!                 or "HERSE1" 0x03 reason, then close              (plaintext)
//! ```
//!
//! Both sides derive per-connection keys from the link secret and both
//! nonces:
//!
//! ```text
//! okm = HKDF-SHA256(ikm  = secret,
//!                   salt = "herse-collab/1" || client_nonce || server_nonce,
//!                   info = "herse-collab/1 keys " || session_id,  L = 64)
//! c2s = okm[0..32]   s2c = okm[32..64]
//! ```
//!
//! The host holds two secrets (edit and view-only) and tries the guest's
//! first frame with the keys of each: the one that authenticates decides the
//! guest's role, and a guest whose frame authenticates under neither is
//! rejected. The guest checks the host's Welcome the same way, so both sides
//! prove they know the secret without sending it, and fresh nonces make every
//! connection's keys unique (no replay across connections).
//!
//! # Frames
//!
//! Every later message is `nonce[24] || XChaCha20-Poly1305(key, nonce,
//! plaintext, aad = "HRS1" || direction || seq_be64)`, with a random nonce per
//! frame and a per-direction sequence number that must increase by exactly
//! one: tampered, replayed, reordered or dropped frames fail authentication
//! and the connection is closed. The plaintext is one y-sync protocol
//! message (sync step 1/2, update, awareness, or a Herse control message).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::util::random_bytes;

/// Protocol magic at the start of plaintext handshake messages.
pub(crate) const MAGIC: &[u8; 6] = b"HERSE1";
/// Wire protocol version.
pub(crate) const VERSION: u8 = 1;
pub(crate) const HELLO: u8 = 0x01;
pub(crate) const CHALLENGE: u8 = 0x02;
pub(crate) const REJECT: u8 = 0x03;
/// Reject reasons.
pub(crate) const REJECT_AUTH: u8 = 1;
pub(crate) const REJECT_VERSION: u8 = 2;

pub(crate) const NONCE_LEN: usize = 32;
const FRAME_NONCE: usize = 24;
const TAG: usize = 16;
/// Smallest valid sealed frame (empty plaintext).
pub(crate) const MIN_FRAME: usize = FRAME_NONCE + TAG;

pub(crate) type HandshakeNonce = [u8; NONCE_LEN];

/// `MAGIC || kind || VERSION || nonce`.
pub(crate) fn hello_message(kind: u8, nonce: &HandshakeNonce) -> Vec<u8> {
    let mut m = Vec::with_capacity(MAGIC.len() + 2 + NONCE_LEN);
    m.extend_from_slice(MAGIC);
    m.push(kind);
    m.push(VERSION);
    m.extend_from_slice(nonce);
    m
}

pub(crate) fn reject_message(reason: u8) -> Vec<u8> {
    let mut m = MAGIC.to_vec();
    m.push(REJECT);
    m.push(reason);
    m
}

/// What a plaintext handshake message is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandshakeMsg {
    Hello(HandshakeNonce),
    Challenge(HandshakeNonce),
    Reject(u8),
    /// A Herse message of another protocol version.
    WrongVersion(u8),
}

pub(crate) fn parse_handshake(m: &[u8]) -> Option<HandshakeMsg> {
    let rest = m.strip_prefix(MAGIC.as_slice())?;
    match *rest {
        [REJECT, reason] => Some(HandshakeMsg::Reject(reason)),
        [kind @ (HELLO | CHALLENGE), version, ref nonce @ ..] => {
            if version != VERSION {
                return Some(HandshakeMsg::WrongVersion(version));
            }
            let nonce: HandshakeNonce = nonce.try_into().ok()?;
            Some(if kind == HELLO { HandshakeMsg::Hello(nonce) } else { HandshakeMsg::Challenge(nonce) })
        }
        _ => None,
    }
}

pub(crate) fn new_nonce() -> HandshakeNonce {
    random_bytes::<NONCE_LEN>()
}

/// Per-connection keys `(client-to-server, server-to-client)`.
pub(crate) fn derive_keys(secret: &[u8; 32], session_id: &str, client: &HandshakeNonce, server: &HandshakeNonce) -> ([u8; 32], [u8; 32]) {
    let mut salt = Vec::with_capacity(14 + 2 * NONCE_LEN);
    salt.extend_from_slice(b"herse-collab/1");
    salt.extend_from_slice(client);
    salt.extend_from_slice(server);
    let hk = Hkdf::<Sha256>::new(Some(&salt), secret);
    let mut info = b"herse-collab/1 keys ".to_vec();
    info.extend_from_slice(session_id.as_bytes());
    let mut okm = [0u8; 64];
    // 64 bytes is far below HKDF-SHA256's 8160-byte limit.
    hk.expand(&info, &mut okm).expect("HKDF output length");
    let mut c2s = [0u8; 32];
    let mut s2c = [0u8; 32];
    c2s.copy_from_slice(&okm[..32]);
    s2c.copy_from_slice(&okm[32..]);
    (c2s, s2c)
}

const C2S: u8 = 0;
const S2C: u8 = 1;

/// One direction of a connection.
pub(crate) struct Half {
    cipher: XChaCha20Poly1305,
    direction: u8,
    seq: u64,
}

impl Half {
    fn new(key: &[u8; 32], direction: u8) -> Half {
        Half { cipher: XChaCha20Poly1305::new(key.into()), direction, seq: 0 }
    }

    fn aad(&self) -> [u8; 13] {
        let mut aad = [0u8; 13];
        aad[..4].copy_from_slice(b"HRS1");
        aad[4] = self.direction;
        aad[5..].copy_from_slice(&self.seq.to_be_bytes());
        aad
    }

    /// Seal the next frame.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_bytes = random_bytes::<FRAME_NONCE>();
        let nonce = XNonce::from(nonce_bytes);
        let aad = self.aad();
        // Encryption only fails for plaintexts beyond the AEAD's ~256 GiB limit.
        let ct = self.cipher.encrypt(&nonce, Payload { msg: plaintext, aad: &aad }).expect("frame too large to encrypt");
        self.seq += 1;
        let mut out = Vec::with_capacity(FRAME_NONCE + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }

    /// Open the next frame; `None` if it does not authenticate (wrong key,
    /// tampered, replayed, out of order, truncated). The sequence number only
    /// advances on success.
    pub fn open(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < MIN_FRAME {
            return None;
        }
        let (nonce, ct) = frame.split_at(FRAME_NONCE);
        let nonce = XNonce::try_from(nonce).ok()?;
        let aad = self.aad();
        let pt = self.cipher.decrypt(&nonce, Payload { msg: ct, aad: &aad }).ok()?;
        self.seq += 1;
        Some(pt)
    }
}

/// Both directions of a connection, from one side's point of view.
pub(crate) struct Channel {
    pub send: Half,
    pub recv: Half,
}

impl Channel {
    pub fn client(keys: &([u8; 32], [u8; 32])) -> Channel {
        Channel { send: Half::new(&keys.0, C2S), recv: Half::new(&keys.1, S2C) }
    }

    pub fn server(keys: &([u8; 32], [u8; 32])) -> Channel {
        Channel { send: Half::new(&keys.1, S2C), recv: Half::new(&keys.0, C2S) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Channel, Channel) {
        let secret = [9u8; 32];
        let (c, s) = (new_nonce(), new_nonce());
        let keys = derive_keys(&secret, "sess", &c, &s);
        (Channel::client(&keys), Channel::server(&keys))
    }

    #[test]
    fn seal_open_roundtrip_both_ways() {
        let (mut client, mut server) = pair();
        for i in 0..5u8 {
            let f = client.send.seal(&[i; 100]);
            assert_eq!(server.recv.open(&f).unwrap(), vec![i; 100]);
            let g = server.send.seal(b"pong");
            assert_eq!(client.recv.open(&g).unwrap(), b"pong");
        }
        let empty = client.send.seal(b"");
        assert_eq!(server.recv.open(&empty).unwrap(), b"");
    }

    #[test]
    fn rejects_tampering_replay_reorder_and_reflection() {
        let (mut client, mut server) = pair();
        let f0 = client.send.seal(b"zero");
        let f1 = client.send.seal(b"one");
        // Reordered.
        assert!(server.recv.open(&f1).is_none());
        // Tampered (every byte position).
        for i in 0..f0.len() {
            let mut t = f0.clone();
            t[i] ^= 0x40;
            assert!(server.recv.open(&t).is_none(), "byte {i}");
        }
        // Truncated.
        assert!(server.recv.open(&f0[..f0.len() - 1]).is_none());
        assert!(server.recv.open(&f0[..MIN_FRAME - 1]).is_none());
        // The original still opens (failed attempts don't advance the seq)...
        assert_eq!(server.recv.open(&f0).unwrap(), b"zero");
        // ...but not twice.
        assert!(server.recv.open(&f0).is_none());
        assert_eq!(server.recv.open(&f1).unwrap(), b"one");
        // A client frame reflected back at the client does not open.
        let f2 = client.send.seal(b"two");
        assert!(client.recv.open(&f2).is_none());
    }

    #[test]
    fn keys_depend_on_everything() {
        let (c, s) = (new_nonce(), new_nonce());
        let base = derive_keys(&[1; 32], "a", &c, &s);
        assert_ne!(base, derive_keys(&[2; 32], "a", &c, &s));
        assert_ne!(base, derive_keys(&[1; 32], "b", &c, &s));
        assert_ne!(base, derive_keys(&[1; 32], "a", &s, &c));
        assert_ne!(base.0, base.1);
        // Wrong secret cannot open.
        let mut good = Channel::client(&base);
        let mut bad = Channel::server(&derive_keys(&[2; 32], "a", &c, &s));
        assert!(bad.recv.open(&good.send.seal(b"hi")).is_none());
    }

    #[test]
    fn handshake_messages() {
        let n = new_nonce();
        assert_eq!(parse_handshake(&hello_message(HELLO, &n)), Some(HandshakeMsg::Hello(n)));
        assert_eq!(parse_handshake(&hello_message(CHALLENGE, &n)), Some(HandshakeMsg::Challenge(n)));
        assert_eq!(parse_handshake(&reject_message(REJECT_AUTH)), Some(HandshakeMsg::Reject(REJECT_AUTH)));
        let mut v2 = hello_message(HELLO, &n);
        v2[7] = 2;
        assert_eq!(parse_handshake(&v2), Some(HandshakeMsg::WrongVersion(2)));
        assert_eq!(parse_handshake(b"HERSE1"), None);
        assert_eq!(parse_handshake(&hello_message(HELLO, &n)[..20]), None);
        assert_eq!(parse_handshake(b"GET / HTTP/1.1"), None);
    }
}
