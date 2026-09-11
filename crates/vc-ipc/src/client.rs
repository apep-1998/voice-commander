//! The client side of the socket.
//!
//! This code runs on every keypress, between the user pressing a key and audio being
//! captured, so it is deliberately synchronous and dependency-free: a blocking
//! `UnixStream`, one write, one read. No async runtime to spin up, no TLS stack to
//! initialise, no configuration to parse in the common case.
//!
//! It lives in `vc-ipc` rather than in the binary so that the daemon's own integration tests
//! drive it through exactly the code path a keybind does.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::{Command, ErrorCode, Request, Response, PROTOCOL_VERSION};

/// Why talking to the daemon failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing is listening. Almost always "the daemon is not running".
    #[error("no daemon is listening on {path}")]
    NotRunning { path: PathBuf },
    #[error("talking to the daemon on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The daemon closed the connection without answering.
    #[error("the daemon closed the connection without replying")]
    NoReply,
    #[error("could not understand the daemon's reply: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The daemon answered, and the answer was a refusal.
    #[error("{message}")]
    Refused { code: ErrorCode, message: String },
}

impl ClientError {
    /// Process exit code, so a wrapper script can branch without parsing text.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Refused { code, .. } => code.exit_code(),
            Self::NotRunning { .. } => 69, // EX_UNAVAILABLE
            _ => 1,
        }
    }
}

/// A connection to the daemon.
///
/// The reader is held for the connection's whole life rather than being built per call. A
/// `BufReader` reads ahead, so a fresh one per read would silently discard whatever it
/// buffered past the end of the line — which is exactly how a subscription would lose the
/// first event, if the daemon happened to write the acknowledgement and an event together.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    path: PathBuf,
}

impl Client {
    /// Connect, with a timeout on both directions.
    ///
    /// The timeouts matter more than they look: without them a wedged daemon turns every
    /// press of the keybind into a process that hangs forever, and the user ends up with a
    /// screen full of them.
    pub fn connect(path: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning {
                    path: path.to_owned(),
                }
            }
            _ => ClientError::Io {
                path: path.to_owned(),
                source,
            },
        })?;

        let io_error = |source| ClientError::Io {
            path: path.to_owned(),
            source,
        };
        stream.set_read_timeout(Some(timeout)).map_err(io_error)?;
        stream.set_write_timeout(Some(timeout)).map_err(io_error)?;

        let writer = stream.try_clone().map_err(|source| ClientError::Io {
            path: path.to_owned(),
            source,
        })?;

        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            path: path.to_owned(),
        })
    }

    /// Send one command and read one reply.
    pub fn send(&mut self, command: Command) -> Result<Response, ClientError> {
        self.write(command)?;
        match self.read_line()? {
            Some(line) => match serde_json::from_str::<Response>(&line)? {
                Response::Error { code, message } => Err(ClientError::Refused { code, message }),
                response => Ok(response),
            },
            None => Err(ClientError::NoReply),
        }
    }

    /// Send `subscribe` and hand back an iterator over the event lines that follow.
    ///
    /// Yields raw lines rather than parsed envelopes so that a consumer on a newer schema
    /// version than this client can still see everything — the client is not a gatekeeper
    /// for a contract it does not own.
    pub fn subscribe(mut self, events: Vec<String>) -> Result<EventStream, ClientError> {
        self.write(Command::Subscribe { events })?;
        match self.read_line()? {
            Some(line) => match serde_json::from_str::<Response>(&line)? {
                Response::Subscribed => {}
                Response::Error { code, message } => {
                    return Err(ClientError::Refused { code, message })
                }
                other => {
                    return Err(ClientError::Refused {
                        code: ErrorCode::BadRequest,
                        message: format!("expected a subscription, got {other:?}"),
                    })
                }
            },
            None => return Err(ClientError::NoReply),
        }

        // A subscriber may sit silent for hours between recordings, so the read timeout that
        // protects a one-shot command would be actively wrong here.
        self.reader
            .get_ref()
            .set_read_timeout(None)
            .map_err(|source| ClientError::Io {
                path: self.path.clone(),
                source,
            })?;

        // Moving the reader intact is the point: anything already buffered stays buffered.
        Ok(EventStream {
            reader: self.reader,
        })
    }

    fn write(&mut self, command: Command) -> Result<(), ClientError> {
        let mut line = serde_json::to_string(&Request::new(command))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|source| ClientError::Io {
                path: self.path.clone(),
                source,
            })
    }

    fn read_line(&mut self) -> Result<Option<String>, ClientError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|source| ClientError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok((read > 0).then_some(line))
    }
}

/// Event lines arriving from a subscription.
#[derive(Debug)]
pub struct EventStream {
    reader: BufReader<UnixStream>,
}

impl Iterator for EventStream {
    type Item = Result<String, std::io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(Ok(line.trim_end().to_owned())),
            Err(error) => Some(Err(error)),
        }
    }
}

/// Whether a daemon is listening on `path`.
///
/// A socket file existing proves nothing — one left behind by a crash looks identical to a
/// live one until something tries to connect.
pub fn is_running(path: &Path, timeout: Duration) -> bool {
    Client::connect(path, timeout)
        .and_then(|mut client| client.send(Command::Ping))
        .is_ok_and(|response| matches!(response, Response::Pong { .. }))
}

/// Where to reach the daemon.
///
/// Checked in the order a user would expect to be able to override: an explicit argument,
/// then the environment, then the XDG default. Reading it from the environment is what lets
/// the client stay free of configuration parsing on the hot path — a user who moves the
/// socket in `config.toml` exports the same value here.
pub fn resolve_socket_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit
        .or_else(|| std::env::var_os("VOICE_COMMANDER_SOCKET").map(PathBuf::from))
        .unwrap_or_else(vc_core::paths::socket_path)
}

/// The protocol version this client speaks.
pub fn protocol_version() -> u32 {
    PROTOCOL_VERSION
}
