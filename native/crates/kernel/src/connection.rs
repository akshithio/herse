//! Port reservation and connection files.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// The contents of a Jupyter connection file.
#[derive(Clone, Debug, Serialize)]
pub struct ConnectionInfo {
    pub transport: String,
    pub ip: String,
    pub shell_port: u16,
    pub iopub_port: u16,
    pub stdin_port: u16,
    pub control_port: u16,
    pub hb_port: u16,
    pub key: String,
    pub signature_scheme: String,
    pub kernel_name: String,
}

impl ConnectionInfo {
    pub(crate) fn new(kernel_name: &str, key: &str, ports: [u16; 5]) -> Self {
        let [shell_port, iopub_port, stdin_port, control_port, hb_port] = ports;
        ConnectionInfo {
            transport: "tcp".into(),
            ip: Ipv4Addr::LOCALHOST.to_string(),
            shell_port,
            iopub_port,
            stdin_port,
            control_port,
            hb_port,
            key: key.into(),
            signature_scheme: "hmac-sha256".into(),
            kernel_name: kernel_name.into(),
        }
    }

    pub(crate) fn ports(&self) -> [u16; 5] {
        [
            self.shell_port,
            self.iopub_port,
            self.stdin_port,
            self.control_port,
            self.hb_port,
        ]
    }

    pub(crate) fn addr(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }
}

/// Ports handed to kernels of this process that are still alive, so two
/// concurrent launches never get the same port even if the OS would hand it
/// out again after we close our probe listener.
fn ports_in_use() -> &'static Mutex<HashSet<u16>> {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    PORTS.get_or_init(Default::default)
}

/// Reserve five distinct free loopback ports: bind five listeners to port 0
/// at once (so the OS hands out distinct ports), read them, close them.
///
/// There is an unavoidable window between our close and the kernel's bind
/// (the kernel binds ~100-300 ms later, after Python starts). If something
/// else grabs a port in that window the kernel exits with "Address already
/// in use" and the launcher retries with fresh ports.
pub(crate) fn reserve_ports() -> io::Result<[u16; 5]> {
    let mut last_err = None;
    for _ in 0..16 {
        let listeners: io::Result<Vec<TcpListener>> = (0..5)
            .map(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .collect();
        let listeners = match listeners {
            Ok(l) => l,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        let mut ports = [0u16; 5];
        for (slot, l) in ports.iter_mut().zip(&listeners) {
            *slot = l.local_addr()?.port();
        }
        let mut in_use = ports_in_use().lock().unwrap_or_else(|e| e.into_inner());
        if ports.iter().any(|p| in_use.contains(p)) {
            continue;
        }
        in_use.extend(ports);
        return Ok(ports);
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("could not reserve 5 free ports")))
}

pub(crate) fn release_ports(ports: [u16; 5]) {
    let mut in_use = ports_in_use().lock().unwrap_or_else(|e| e.into_inner());
    for p in ports {
        in_use.remove(&p);
    }
}

/// Where connection files live: a private per-user directory under the
/// system temp dir (per-user already on macOS).
pub(crate) fn runtime_dir() -> PathBuf {
    std::env::temp_dir().join("herse-kernels")
}

/// Write `info` to a fresh connection file (mode 0600: it holds the HMAC key).
pub(crate) fn write_connection_file(info: &ConnectionInfo, file_name: &str) -> io::Result<PathBuf> {
    let dir = runtime_dir();
    create_private_dir(&dir)?;
    let path = dir.join(file_name);
    let json = serde_json::to_vec_pretty(info).map_err(io::Error::other)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(&json)?;
    Ok(path)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_ports_are_distinct_across_calls() {
        let a = reserve_ports().unwrap();
        let b = reserve_ports().unwrap();
        let all: HashSet<u16> = a.iter().chain(&b).copied().collect();
        assert_eq!(all.len(), 10);
        release_ports(a);
        release_ports(b);
    }

    #[test]
    fn connection_file_is_private_json() {
        let info = ConnectionInfo::new("python3", "abc", [1, 2, 3, 4, 5]);
        let path =
            write_connection_file(&info, &format!("test-{}.json", std::process::id())).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["shell_port"], 1);
        assert_eq!(v["hb_port"], 5);
        assert_eq!(v["signature_scheme"], "hmac-sha256");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
