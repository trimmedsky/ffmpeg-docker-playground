//! Where the HTTP API listens: a TCP address or a Unix domain socket.
//!
//! `FFMPEG_AGENT_LISTEN` is either `<ip>:<port>` or `unix:<absolute path>`. A Unix socket
//! suits a supervisor that connects to the backend through a mounted directory instead of
//! a network address (for example a host agent that relays authenticated requests).

use std::{fmt, net::SocketAddr, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl Listen {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(path) = value.strip_prefix("unix:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() || path.file_name().is_none() {
                return Err("FFMPEG_AGENT_LISTEN unix: needs an absolute socket path".into());
            }
            if !cfg!(unix) {
                return Err("FFMPEG_AGENT_LISTEN unix: is not supported on this platform".into());
            }
            return Ok(Self::Unix(path));
        }
        value
            .parse()
            .map(Self::Tcp)
            .map_err(|_| "invalid listen address (use <ip>:<port> or unix:<absolute path>)".into())
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => write!(f, "{address}"),
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

pub enum Listener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, unix::SocketFile),
}

pub async fn bind(listen: &Listen) -> Result<Listener, String> {
    match listen {
        Listen::Tcp(address) => tokio::net::TcpListener::bind(address)
            .await
            .map(Listener::Tcp)
            .map_err(|_| "cannot bind listen address".into()),
        #[cfg(unix)]
        Listen::Unix(path) => unix::bind(path).map(|(l, f)| Listener::Unix(l, f)),
        #[cfg(not(unix))]
        Listen::Unix(_) => unreachable!("rejected by Listen::parse"),
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs::{self, Permissions},
        io::ErrorKind,
        os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };

    /// The socket this process created; removed on drop if the path still refers to it.
    pub struct SocketFile {
        path: PathBuf,
        identity: (u64, u64),
    }

    impl Drop for SocketFile {
        fn drop(&mut self) {
            if fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    /// Binds `path`, replacing a socket left behind by a previous process. A socket that
    /// still accepts connections belongs to a live process and is left alone, as is any
    /// path that is not a socket. The socket is mode 0666: who may connect is decided by
    /// the directory that holds it, and every request is authenticated anyway.
    pub fn bind(path: &Path) -> Result<(tokio::net::UnixListener, SocketFile), String> {
        let shown = path.display();
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => {
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => return Err(format!("listen socket {shown} is in use")),
                    Err(e) if e.kind() == ErrorKind::ConnectionRefused => fs::remove_file(path)
                        .map_err(|e| format!("cannot remove stale socket {shown}: {e}"))?,
                    Err(e) => return Err(format!("cannot check existing socket {shown}: {e}")),
                }
            }
            Ok(_) => return Err(format!("listen path {shown} exists and is not a socket")),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect listen path {shown}: {e}")),
        }
        let listener = tokio::net::UnixListener::bind(path)
            .map_err(|e| format!("cannot bind listen socket {shown}: {e}"))?;
        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("cannot inspect listen socket {shown}: {e}"))?;
        let file = SocketFile {
            path: path.to_path_buf(),
            identity: (meta.dev(), meta.ino()),
        };
        fs::set_permissions(path, Permissions::from_mode(0o666))
            .map_err(|e| format!("cannot set mode of listen socket {shown}: {e}"))?;
        Ok((listener, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tcp_and_unix_addresses() {
        assert_eq!(
            Listen::parse("127.0.0.1:8080"),
            Ok(Listen::Tcp("127.0.0.1:8080".parse().unwrap()))
        );
        assert_eq!(
            Listen::parse("[::1]:80"),
            Ok(Listen::Tcp("[::1]:80".parse().unwrap()))
        );
        assert_eq!(
            Listen::parse("unix:/run/hng-backend/ffmpeg.sock"),
            Ok(Listen::Unix("/run/hng-backend/ffmpeg.sock".into()))
        );
        assert_eq!(
            Listen::parse("unix:/run/a.sock").unwrap().to_string(),
            "unix:/run/a.sock"
        );
        for invalid in [
            "",
            "8080",
            "localhost:8080",
            "unix:",
            "unix:relative.sock",
            "unix:/",
        ] {
            assert!(Listen::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaces_only_a_stale_socket() {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        // A socket whose listener is gone, as after a crash.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let Listener::Unix(listener, file) = bind(&Listen::Unix(path.clone())).await.unwrap()
        else {
            panic!("not a Unix listener");
        };
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o666);
        let Err(error) = bind(&Listen::Unix(path.clone())).await else {
            panic!("a live socket was replaced");
        };
        assert!(error.contains("in use"), "{error}");
        drop(listener);
        drop(file);
        assert!(!path.exists(), "socket not removed on drop");

        let other = dir.path().join("regular");
        std::fs::write(&other, b"data").unwrap();
        let Err(error) = bind(&Listen::Unix(other.clone())).await else {
            panic!("a regular file was replaced");
        };
        assert!(error.contains("not a socket"), "{error}");
        assert_eq!(std::fs::read(&other).unwrap(), b"data");
    }
}
