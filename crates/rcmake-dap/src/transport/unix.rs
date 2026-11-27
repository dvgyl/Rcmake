//! POSIX `AF_UNIX` stream socket transport (`cmDebuggerPipeConnection_POSIX`).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_host::Host;

use super::{Connection, Listener, error_text};

/// Size of `sockaddr_un::sun_path`, including the terminating NUL.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const SUN_PATH_BYTES: usize = 104;
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
const SUN_PATH_BYTES: usize = 108;

/// Time limit for one write to the client. A client that does not read for this time
/// ends the session.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// A socket bound at a path.
pub struct UnixEndpoint {
    listener: UnixListener,
    path: PathBuf,
    host: Arc<dyn Host>,
}

impl UnixEndpoint {
    /// Bind a socket at `path`, as CMake does: the path is cut to fit `sun_path`.
    ///
    /// # Errors
    ///
    /// Return the CMake reason text: `Failed to bind name '<path>' to socket: <error>`.
    pub fn bind(path: &Path, host: Arc<dyn Host>) -> Result<Self, String> {
        let bytes = path.as_os_str().as_bytes();
        let cut = bytes.split(|&byte| byte == 0).next().unwrap_or_default();
        let cut = cut.get(..SUN_PATH_BYTES - 1).unwrap_or(cut);
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(cut));
        match UnixListener::bind(&path) {
            Ok(listener) => Ok(Self {
                listener,
                path,
                host,
            }),
            Err(error) => Err(format!(
                "Failed to bind name '{}' to socket: {}",
                path.display(),
                error_text(&error)
            )),
        }
    }

    /// Path of the socket.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Listener for UnixEndpoint {
    fn accept(self: Box<Self>) -> io::Result<Box<dyn Connection>> {
        let accepted = self.listener.accept();
        // The listener is no longer needed after `accept`, so remove its name.
        drop(self.listener);
        let _ = self.host.remove_file(&self.path);
        let (stream, _) = accepted?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(Box::new(UnixConnection { stream }))
    }
}

struct UnixConnection {
    stream: UnixStream,
}

impl Connection for UnixConnection {
    fn reader(&self) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.stream.try_clone()?))
    }

    fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.stream.try_clone()?))
    }

    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
