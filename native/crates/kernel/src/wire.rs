//! Jupyter wire protocol v5.3 framing and HMAC signing.
//!
//! A message on the wire is
//! `[routing...] <IDS|MSG> <hmac> <header> <parent> <metadata> <content> [buffers...]`.
//! The HMAC covers exactly the four JSON frames, in order. Routing frames
//! (SUB topics on iopub) precede the delimiter, so we search for it.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::util;
use crate::zmtp;

pub(crate) const DELIMITER: &[u8] = b"<IDS|MSG>";
pub(crate) const PROTOCOL_VERSION: &str = "5.3";

type HmacSha256 = Hmac<Sha256>;

/// Signs outgoing and verifies incoming messages for one kernel session.
#[derive(Clone)]
pub(crate) struct Signer {
    /// `None` when the key is empty (signing disabled).
    mac: Option<HmacSha256>,
    session: String,
    username: String,
}

#[derive(Serialize)]
struct HeaderOut<'a> {
    msg_id: &'a str,
    session: &'a str,
    username: &'a str,
    date: &'a str,
    msg_type: &'a str,
    version: &'a str,
}

impl Signer {
    pub fn new(key: &[u8], session: String, username: String) -> Self {
        let mac = (!key.is_empty())
            .then(|| HmacSha256::new_from_slice(key).expect("HMAC accepts any key length"));
        Signer {
            mac,
            session,
            username,
        }
    }

    fn signature(&self, parts: [&[u8]; 4]) -> String {
        let Some(mac) = &self.mac else {
            return String::new();
        };
        let mut mac = mac.clone();
        for part in parts {
            mac.update(part);
        }
        let digest = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            util::push_hex(&mut hex, byte);
        }
        hex
    }

    fn verify(&self, parts: [&[u8]; 4], signature: &[u8]) -> bool {
        let Some(mac) = &self.mac else { return true };
        let Some(signature) = util::decode_hex(signature) else {
            return false;
        };
        let mut mac = mac.clone();
        for part in parts {
            mac.update(part);
        }
        mac.verify_slice(&signature).is_ok()
    }

    /// Build a signed message and return it in ZMTP wire format, ready to
    /// write to a DEALER connection. `parent` is the raw parent header JSON
    /// (`{}` for requests).
    pub fn encode(&self, msg_type: &str, msg_id: &str, parent: &[u8], content: &[u8]) -> Vec<u8> {
        let date = util::now_iso8601();
        let header = serde_json::to_vec(&HeaderOut {
            msg_id,
            session: &self.session,
            username: &self.username,
            date: &date,
            msg_type,
            version: PROTOCOL_VERSION,
        })
        .expect("header serializes");
        let metadata: &[u8] = b"{}";
        let signature = self.signature([&header, parent, metadata, content]);
        let mut out = Vec::with_capacity(content.len() + header.len() + 160);
        zmtp::encode_message(
            &mut out,
            &[
                DELIMITER,
                signature.as_bytes(),
                &header,
                parent,
                metadata,
                content,
            ],
        );
        out
    }
}

/// A decoded, verified incoming message.
#[derive(Debug)]
pub(crate) struct Message {
    #[allow(dead_code)]
    pub msg_id: String,
    pub msg_type: String,
    pub parent_msg_id: Option<String>,
    /// The header exactly as received; used verbatim as the parent of replies
    /// we send back (input_reply).
    pub raw_header: Vec<u8>,
    pub content: Value,
    #[allow(dead_code)]
    pub buffers: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) enum DecodeError {
    NoDelimiter,
    Truncated,
    BadSignature,
    Json(serde_json::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::NoDelimiter => f.write_str("no <IDS|MSG> delimiter"),
            DecodeError::Truncated => f.write_str("missing JSON frames"),
            DecodeError::BadSignature => f.write_str("HMAC signature mismatch"),
            DecodeError::Json(e) => write!(f, "invalid JSON: {e}"),
        }
    }
}

#[derive(Deserialize)]
struct HeaderIn {
    msg_id: String,
    msg_type: String,
}

#[derive(Deserialize)]
struct ParentIn {
    #[serde(default)]
    msg_id: Option<String>,
}

pub(crate) fn decode(frames: Vec<Vec<u8>>, signer: &Signer) -> Result<Message, DecodeError> {
    let delim = frames
        .iter()
        .position(|f| f == DELIMITER)
        .ok_or(DecodeError::NoDelimiter)?;
    if frames.len() < delim + 6 {
        return Err(DecodeError::Truncated);
    }
    let mut frames = frames.into_iter().skip(delim + 1);
    let signature = frames.next().unwrap_or_default();
    let header = frames.next().unwrap_or_default();
    let parent = frames.next().unwrap_or_default();
    let metadata = frames.next().unwrap_or_default();
    let content = frames.next().unwrap_or_default();
    if !signer.verify([&header, &parent, &metadata, &content], &signature) {
        return Err(DecodeError::BadSignature);
    }
    let head: HeaderIn = serde_json::from_slice(&header).map_err(DecodeError::Json)?;
    let parent_msg_id = serde_json::from_slice::<ParentIn>(&parent)
        .ok()
        .and_then(|p| p.msg_id)
        .filter(|id| !id.is_empty());
    let content = if content.is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_slice(&content).map_err(DecodeError::Json)?
    };
    Ok(Message {
        msg_id: head.msg_id,
        msg_type: head.msg_type,
        parent_msg_id,
        raw_header: header,
        content,
        buffers: frames.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(wire: &[u8]) -> Vec<Vec<u8>> {
        smol::block_on(async {
            match zmtp::read(&mut &wire[..]).await.unwrap() {
                zmtp::Incoming::Message(f) => f,
                zmtp::Incoming::Command { .. } => panic!(),
            }
        })
    }

    #[test]
    fn signature_matches_python_hmac() {
        // hmac.new(b"secret-key", b'{"a":1}' + b'{}' + b'{}' + b'{"code":"1"}', sha256)
        let s = Signer::new(b"secret-key", "s".into(), "u".into());
        assert_eq!(
            s.signature([br#"{"a":1}"#, b"{}", b"{}", br#"{"code":"1"}"#]),
            "cf91902d477e8b9a31e8154a9f781c352fd929b750ed1763608ba4c2d4a5c996"
        );
    }

    #[test]
    fn encode_decode_roundtrip_with_topic_prefix() {
        let s = Signer::new(b"k", "sess".into(), "me".into());
        let wire = s.encode(
            "execute_request",
            "id-1",
            br#"{"msg_id":"p-1"}"#,
            br#"{"code":"1+1"}"#,
        );
        let mut frames = frames_of(&wire);
        frames.insert(0, b"kernel.abc.status".to_vec()); // iopub topic
        let msg = decode(frames, &s).unwrap();
        assert_eq!(msg.msg_id, "id-1");
        assert_eq!(msg.msg_type, "execute_request");
        assert_eq!(msg.parent_msg_id.as_deref(), Some("p-1"));
        assert_eq!(msg.content["code"], "1+1");
        let header: Value = serde_json::from_slice(&msg.raw_header).unwrap();
        assert_eq!(header["session"], "sess");
        assert_eq!(header["version"], "5.3");
    }

    #[test]
    fn rejects_bad_signature() {
        let s = Signer::new(b"k", "sess".into(), "me".into());
        let other = Signer::new(b"other", "sess".into(), "me".into());
        let wire = other.encode("status", "id", b"{}", b"{}");
        assert!(matches!(
            decode(frames_of(&wire), &s),
            Err(DecodeError::BadSignature)
        ));
        let mut frames = frames_of(&wire);
        frames.retain(|f| f != DELIMITER);
        assert!(matches!(decode(frames, &s), Err(DecodeError::NoDelimiter)));
    }
}
