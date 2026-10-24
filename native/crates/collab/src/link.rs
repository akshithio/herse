//! Join links.
//!
//! ```text
//! herse://join/<host>:<port>[,<host>:<port>...]/<session-id>#<secret>
//! herse://join/100.101.102.103:41234,192.168.1.20:41234,ada-mbp.local:41234/3f9c2a7d1e5b4c60#Qm9i...
//! ```
//!
//! The link is the capability: whoever has it can join. The fragment holds
//! the 32-byte session secret (base64url); it is never sent over the wire,
//! only used to derive per-connection keys (see [`crate::crypto`]). A
//! view-only link's fragment is `ro.<secret>` with a different secret, which
//! the host only accepts for read-only access (the `ro.` marker is just for
//! display: the host decides the role from which secret authenticates).
//!
//! A link may list several addresses (Tailscale, LAN, mDNS name); joining
//! tries them all and uses the first that answers. [`JoinLink::parse`] also
//! accepts the plain form without `herse://join/` (handy when the scheme gets
//! lost in copy/paste), surrounding whitespace, quotes or angle brackets, and
//! IPv6 hosts in brackets (`[fd7a:115c::5]:41234`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::util::{b64url_decode, b64url_encode};

pub(crate) const SECRET_LEN: usize = 32;

/// A parsed join link.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinLink {
    /// Where the host listens, best first.
    pub hosts: Vec<HostPort>,
    /// Identifies the session (not secret).
    pub session_id: String,
    /// The link was made for view-only access (informational).
    pub view_only: bool,
    secret: [u8; SECRET_LEN],
}

/// One `host:port` of a link. `host` is an IP address or a DNS / mDNS name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl JoinLink {
    pub(crate) fn new(hosts: Vec<HostPort>, session_id: String, secret: [u8; SECRET_LEN], view_only: bool) -> JoinLink {
        JoinLink { hosts, session_id, view_only, secret }
    }

    pub(crate) fn secret(&self) -> &[u8; SECRET_LEN] {
        &self.secret
    }

    /// The same link with other addresses (e.g. after the network changed).
    pub fn with_hosts(&self, hosts: Vec<HostPort>) -> JoinLink {
        JoinLink { hosts, ..self.clone() }
    }

    /// The link without the `herse://join/` prefix.
    pub fn plain(&self) -> String {
        let hosts: Vec<String> = self.hosts.iter().map(ToString::to_string).collect();
        let marker = if self.view_only { "ro." } else { "" };
        format!("{}/{}#{marker}{}", hosts.join(","), self.session_id, b64url_encode(&self.secret))
    }

    /// Parse a link (see module docs for the accepted forms).
    pub fn parse(input: &str) -> Result<JoinLink> {
        let bad = |why: &str| Error::BadLink(why.into());
        let mut s = input.trim();
        for (open, close) in [('<', '>'), ('"', '"'), ('\'', '\''), ('`', '`')] {
            if let Some(inner) = s.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
                s = inner.trim();
            }
        }
        if let Some(rest) = s.strip_prefix("herse://") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("herse:") {
            s = rest.trim_start_matches('/');
        } else if s.contains("://") {
            return Err(bad("not a herse:// link"));
        }
        s = s.strip_prefix("join/").unwrap_or(s);
        let (path, fragment) = s.split_once('#').ok_or_else(|| bad("missing the #secret part"))?;
        let (view_only, secret_text) = match fragment.strip_prefix("ro.") {
            Some(rest) => (true, rest),
            None => (false, fragment),
        };
        let secret_bytes = b64url_decode(secret_text.trim()).ok_or_else(|| bad("the secret is not valid base64url"))?;
        let secret: [u8; SECRET_LEN] = secret_bytes.try_into().map_err(|_| bad("the secret has the wrong length"))?;
        let path = path.trim_end_matches('/');
        let (hosts_text, session_id) = path.rsplit_once('/').ok_or_else(|| bad("missing the session id"))?;
        if session_id.is_empty() || session_id.len() > 64 || !session_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(bad("invalid session id"));
        }
        let hosts = hosts_text.split(',').map(|h| parse_host_port(h.trim())).collect::<Result<Vec<_>>>()?;
        if hosts.is_empty() {
            return Err(bad("no host address"));
        }
        Ok(JoinLink { hosts, session_id: session_id.into(), view_only, secret })
    }
}

fn parse_host_port(s: &str) -> Result<HostPort> {
    let bad = |why: String| Error::BadLink(why);
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once("]:").ok_or_else(|| bad(format!("bad address {s:?}")))?;
        h.parse::<std::net::Ipv6Addr>().map_err(|_| bad(format!("bad IPv6 address {h:?}")))?;
        (h, p)
    } else {
        let (h, p) = s.rsplit_once(':').ok_or_else(|| bad(format!("address {s:?} has no port")))?;
        let valid_name = !h.is_empty()
            && h.len() <= 253
            && h.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
        if !valid_name {
            return Err(bad(format!("bad host {h:?}")));
        }
        (h, p)
    };
    let port: u16 = port.parse().map_err(|_| bad(format!("bad port in {s:?}")))?;
    if port == 0 {
        return Err(bad(format!("bad port in {s:?}")));
    }
    Ok(HostPort { host: host.into(), port })
}

impl fmt::Display for JoinLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "herse://join/{}", self.plain())
    }
}

/// Debug never shows the secret (links end up in logs).
impl fmt::Debug for JoinLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinLink")
            .field("hosts", &self.hosts)
            .field("session_id", &self.session_id)
            .field("view_only", &self.view_only)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl FromStr for JoinLink {
    type Err = Error;
    fn from_str(s: &str) -> Result<JoinLink> {
        JoinLink::parse(s)
    }
}

/// How another machine could reach this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddressKind {
    /// A Tailscale tailnet address (100.64.0.0/10): works from anywhere on
    /// the tailnet.
    Tailscale,
    /// A private LAN IPv4 address (10/8, 172.16/12, 192.168/16).
    Lan,
    /// Another non-loopback IPv4 address (public, or an unusual VPN).
    Other,
    /// `<hostname>.local` (mDNS/Bonjour): works on the same LAN even if the
    /// DHCP address changes.
    Mdns,
}

/// One advertised address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddress {
    pub host: String,
    pub kind: AddressKind,
}

fn classify(ip: Ipv4Addr) -> Option<AddressKind> {
    let o = ip.octets();
    if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
        None
    } else if o[0] == 100 && (64..128).contains(&o[1]) {
        Some(AddressKind::Tailscale)
    } else if ip.is_private() {
        Some(AddressKind::Lan)
    } else {
        Some(AddressKind::Other)
    }
}

/// This machine's reachable addresses, best first: Tailscale, LAN IPv4,
/// other IPv4, then `<hostname>.local`.
pub fn local_addresses() -> Vec<LocalAddress> {
    let mut out: Vec<LocalAddress> = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            use if_addrs::IfOperStatus::{Down, LowerLayerDown, NotPresent};
            if matches!(iface.oper_status, Down | NotPresent | LowerLayerDown) {
                continue;
            }
            if let IpAddr::V4(ip) = iface.ip()
                && let Some(kind) = classify(ip)
                && !out.iter().any(|a| a.host == ip.to_string())
            {
                out.push(LocalAddress { host: ip.to_string(), kind });
            }
        }
    }
    if let Some(name) = hostname() {
        out.push(LocalAddress { host: name, kind: AddressKind::Mdns });
    }
    out.sort_by_key(|a| a.kind);
    out
}

/// `<hostname>.local`, from the system host name.
pub(crate) fn hostname() -> Option<String> {
    let raw = system_hostname()?;
    let label = raw.trim().trim_end_matches('.').split('.').next()?.to_owned();
    let label: String = label.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if label.is_empty() || label.eq_ignore_ascii_case("localhost") {
        return None;
    }
    Some(format!("{label}.local"))
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes at most
    // that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// `host:port` entries for this machine's addresses.
pub(crate) fn advertised(port: u16) -> Vec<HostPort> {
    local_addresses().into_iter().map(|a| HostPort { host: a.host, port }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> JoinLink {
        JoinLink::new(
            vec![
                HostPort { host: "100.101.102.103".into(), port: 41234 },
                HostPort { host: "192.168.1.20".into(), port: 41234 },
                HostPort { host: "fd7a:115c:a1e0::5".into(), port: 41234 },
                HostPort { host: "ada-mbp.local".into(), port: 41234 },
            ],
            "3f9c2a7d1e5b4c60".into(),
            [7u8; SECRET_LEN],
            false,
        )
    }

    #[test]
    fn roundtrip_and_plain_forms() {
        let l = link();
        let s = l.to_string();
        assert!(s.starts_with("herse://join/100.101.102.103:41234,192.168.1.20:41234,[fd7a:115c:a1e0::5]:41234,ada-mbp.local:41234/3f9c2a7d1e5b4c60#"));
        assert_eq!(JoinLink::parse(&s).unwrap(), l);
        assert_eq!(JoinLink::parse(&l.plain()).unwrap(), l);
        assert_eq!(JoinLink::parse(&format!("  <{s}>\n")).unwrap(), l);
        assert_eq!(JoinLink::parse(&format!("join/{}", l.plain())).unwrap(), l);
        assert_eq!(JoinLink::parse(&format!("\"{s}\"")).unwrap(), l);

        let mut ro = l.clone();
        ro.view_only = true;
        assert!(ro.to_string().contains("#ro."));
        assert_eq!(JoinLink::parse(&ro.to_string()).unwrap(), ro);
    }

    #[test]
    fn debug_redacts_secret() {
        let l = link();
        let dbg = format!("{l:?}");
        assert!(!dbg.contains(&b64url_encode(&[7u8; SECRET_LEN])));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn rejects_bad_links() {
        let good = link().to_string();
        let secret = good.split('#').nth(1).unwrap().to_owned();
        for bad in [
            "".to_owned(),
            "https://example.com/x#abc".to_owned(),
            "herse://join/1.2.3.4:5/abc".to_owned(),
            format!("herse://join/1.2.3.4/abc#{secret}"),
            format!("herse://join/1.2.3.4:0/abc#{secret}"),
            format!("herse://join/1.2.3.4:99999/abc#{secret}"),
            format!("herse://join/1.2.3.4:5/#{secret}"),
            format!("herse://join/1.2.3.4:5/a b#{secret}"),
            "herse://join/1.2.3.4:5/abc#tooshort".to_owned(),
            format!("herse://join/[zz::1]:5/abc#{secret}"),
            format!("herse://join/bad host:5/abc#{secret}"),
        ] {
            assert!(JoinLink::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn classifies_addresses() {
        assert_eq!(classify("100.64.0.1".parse().unwrap()), Some(AddressKind::Tailscale));
        assert_eq!(classify("100.127.255.254".parse().unwrap()), Some(AddressKind::Tailscale));
        assert_eq!(classify("100.128.0.1".parse().unwrap()), Some(AddressKind::Other));
        assert_eq!(classify("192.168.1.2".parse().unwrap()), Some(AddressKind::Lan));
        assert_eq!(classify("10.0.0.2".parse().unwrap()), Some(AddressKind::Lan));
        assert_eq!(classify("127.0.0.1".parse().unwrap()), None);
        assert_eq!(classify("169.254.3.4".parse().unwrap()), None);
        // Whatever this machine has, the list is ordered and well-formed.
        let addrs = local_addresses();
        assert!(addrs.windows(2).all(|w| w[0].kind <= w[1].kind));
        for a in addrs {
            let hp = HostPort { host: a.host.clone(), port: 1 };
            assert!(parse_host_port(&hp.to_string()).is_ok(), "{a:?}");
        }
    }
}
