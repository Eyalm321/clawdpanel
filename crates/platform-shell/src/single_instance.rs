//! Single-instance lock + reveal-ping IPC. Mirrors the Go Wails
//! `SingleInstanceOptions` / `OnSecondInstanceLaunch`: a second launch fails to
//! take the lock, pings the running instance (which re-reveals the bar) and
//! exits immediately, so we never end up with two bars / two tray icons.
//!
//! On Linux the lock *is* a bound abstract-namespace Unix socket: binding it
//! both reserves the name (it auto-frees when the process dies — no stale lock
//! files) and serves as the reveal-ping channel. A second instance fails to bind
//! (`AddrInUse`), connects, sends `reveal`, and exits.

/// Held by the first instance to keep the listener socket bound for the life of
/// the process. Dropping it stops the accept loop.
pub struct SingleInstance {
    _private: (),
}

#[cfg(target_os = "linux")]
pub fn acquire<F>(id: &str, on_reveal: F) -> std::io::Result<Option<SingleInstance>>
where
    F: Fn() + Send + 'static,
{
    use std::io::{Read, Write};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

    let addr = SocketAddr::from_abstract_name(id.as_bytes())?;

    match UnixListener::bind_addr(&addr) {
        Ok(listener) => {
            // We are the first instance. Serve reveal pings until the process
            // exits (a daemon thread; the bound socket holds the lock).
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(mut s) => {
                            let mut buf = [0u8; 16];
                            if let Ok(n) = s.read(&mut buf) {
                                if buf[..n].starts_with(b"reveal") {
                                    on_reveal();
                                }
                            }
                        }
                        Err(_) => continue,
                    }
                }
            });
            Ok(Some(SingleInstance { _private: () }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Another instance holds the lock — ping it to reveal, then signal
            // the caller to exit. Best-effort: if the connect/write races a
            // shutdown we still exit (the user can relaunch).
            if let Ok(mut stream) = UnixStream::connect_addr(&addr) {
                let _ = stream.write_all(b"reveal");
            }
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Non-Linux fallback: no cross-process locking (this slice targets Linux). Acts
/// as if always the first instance so the app still launches.
#[cfg(not(target_os = "linux"))]
pub fn acquire<F>(_id: &str, _on_reveal: F) -> std::io::Result<Option<SingleInstance>>
where
    F: Fn() + Send + 'static,
{
    Ok(Some(SingleInstance { _private: () }))
}
