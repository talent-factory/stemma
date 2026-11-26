//! A scripted responder on a loopback socket, in place of a hub (see the crate documentation).
//!
//! The responder accepts one connection per action, reads one request from it, records the
//! request, and does what the action says: answer bytes that the test wrote, answer part of
//! them and then wait, wait without an answer, or close. Every answer closes the connection, so
//! each request of the client opens a new one and meets the next action.

#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use stemma_core::protocol::RepositoryId;
use stemma_core::remote::{CaFile, RemoteAddress, RemoteConfig};

/// The path of the repository below the responder.
pub const REPOSITORY_PATH: &str = "/talent-factory/stemma";

/// How long the responder holds a connection when an action says to wait.
const HOLD: Duration = Duration::from_secs(20);

/// What the responder does with one connection.
pub enum Action {
    /// Reads the request and answers these bytes, then closes the connection.
    Respond(Vec<u8>),
    /// Reads the request, answers these bytes, and then holds the connection without a further
    /// byte.
    Partial(Vec<u8>),
    /// Reads the request and holds the connection without an answer.
    Stall,
    /// Reads the request and closes the connection without an answer.
    Close,
    /// Reads the head of the request alone and holds the connection, so that the body of the
    /// client fills the buffers of the socket and its writes wait.
    StallAfterHead,
    /// Accepts the connection and holds it without reading or writing a byte, so that a client
    /// with TLS waits in its handshake.
    Mute,
}

/// A request as the responder received it.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The method.
    pub method: String,
    /// The path.
    pub path: String,
    /// The headers, with their names in lower case, in their order.
    pub headers: Vec<(String, String)>,
    /// The body, with its chunks joined.
    pub body: Vec<u8>,
    /// The bytes of the head as they arrived, for the checks of what was sent.
    pub raw_head: String,
}

impl Recorded {
    /// The value of the first header of this name (in lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(found, _)| found == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A responder that runs in a thread of its own until its actions are done.
pub struct Responder {
    address: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Responder {
    /// A responder without TLS, at `http://127.0.0.1:<port>/talent-factory/stemma`.
    pub fn start(actions: Vec<Action>) -> Self {
        Self::run(actions, None, "127.0.0.1")
    }

    /// A responder with TLS and the certificate of the test CA, at
    /// `https://127.0.0.1:<port>/talent-factory/stemma`. The certificate names the address as well
    /// as `localhost`; the address keeps the client from trying `::1` first.
    pub fn start_tls(actions: Vec<Action>) -> Self {
        Self::run(actions, Some(server_config()), "127.0.0.1")
    }

    fn run(actions: Vec<Action>, tls: Option<Arc<rustls::ServerConfig>>, host: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        let scheme = if tls.is_some() { "https" } else { "http" };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for action in actions {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
                let tls = tls.clone();
                // One thread per connection, so that a held connection does not keep the next
                // one from being accepted.
                thread::spawn(move || {
                    let _ = match tls {
                        None => serve(stream, action, &recorded),
                        Some(config) => {
                            let connection = rustls::ServerConnection::new(config).unwrap();
                            serve(
                                rustls::StreamOwned::new(connection, stream),
                                action,
                                &recorded,
                            )
                        }
                    };
                });
            }
        });
        Self {
            address: format!("{scheme}://{host}:{port}{REPOSITORY_PATH}"),
            requests,
        }
    }

    /// The address of the repository.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The configuration of a remote of this responder, without a CA file.
    pub fn remote(&self) -> RemoteConfig {
        remote(&self.address, None)
    }

    /// The requests that arrived so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// The configuration of a remote at this address, with this CA file.
pub fn remote(address: &str, ca_file: Option<&Path>) -> RemoteConfig {
    let mut config = RemoteConfig::new(
        RemoteAddress::try_new(address).unwrap(),
        RepositoryId::from_bytes([7; 16]),
    );
    config.ca_file = ca_file.map(|path| CaFile::try_new(path.to_str().unwrap()).unwrap());
    config
}

/// The directory of the test CA.
pub fn test_ca() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/test-ca")
}

/// An answer: the status line, the headers, `Content-Length`, `Connection: close`, and the body.
pub fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status} Status\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// An answer with a message of the protocol.
pub fn message(body: &[u8]) -> Vec<u8> {
    response(
        200,
        &[("Content-Type", stemma_remote::MESSAGE_MEDIA_TYPE)],
        body,
    )
}

fn serve<S: Read + Write>(
    stream: S,
    action: Action,
    recorded: &Mutex<Vec<Recorded>>,
) -> io::Result<()> {
    if let Action::Mute = action {
        thread::sleep(HOLD);
        drop(stream);
        return Ok(());
    }
    let mut reader = BufReader::new(stream);
    if let Action::StallAfterHead = action {
        if let Some(head) = read_head(&mut reader) {
            recorded.lock().unwrap().push(head);
        }
        thread::sleep(HOLD);
        return Ok(());
    }
    let request = read_request(&mut reader);
    if let Some(request) = request {
        recorded.lock().unwrap().push(request);
    }
    let stream = reader.get_mut();
    match action {
        Action::Respond(bytes) => {
            stream.write_all(&bytes)?;
            stream.flush()
        }
        Action::Partial(bytes) => {
            stream.write_all(&bytes)?;
            stream.flush()?;
            thread::sleep(HOLD);
            Ok(())
        }
        Action::Stall => {
            thread::sleep(HOLD);
            Ok(())
        }
        Action::Close => Ok(()),
        Action::StallAfterHead | Action::Mute => unreachable!("handled before the request is read"),
    }
}

/// Reads one request: the head, and a body by `Content-Length` or in chunks. A request that
/// breaks off is recorded with what arrived of it.
fn read_request<R: BufRead>(reader: &mut R) -> Option<Recorded> {
    let mut request = read_head(reader)?;
    if let Some(length) = request.header("content-length") {
        let length: usize = length.parse().ok()?;
        let mut body = vec![0; length];
        let read = read_up_to(reader, &mut body);
        body.truncate(read);
        request.body = body;
    } else if request
        .header("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        request.body = read_chunks(reader);
    }
    Some(request)
}

/// Reads the head of a request, and records it with an empty body.
fn read_head<R: BufRead>(reader: &mut R) -> Option<Recorded> {
    let mut raw_head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        raw_head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let mut lines = raw_head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_owned();
    let path = start.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Some(Recorded {
        method,
        path,
        headers,
        body: Vec::new(),
        raw_head,
    })
}

fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => filled += read,
        }
    }
    filled
}

/// The joined chunks of a body, up to the last chunk or to where the body breaks off.
fn read_chunks<R: BufRead>(reader: &mut R) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return body;
        }
        let size = line.trim().split(';').next().unwrap_or("");
        let Ok(size) = usize::from_str_radix(size, 16) else {
            return body;
        };
        if size == 0 {
            let mut end = String::new();
            let _ = reader.read_line(&mut end);
            return body;
        }
        let mut chunk = vec![0; size];
        let read = read_up_to(reader, &mut chunk);
        body.extend_from_slice(&chunk[..read]);
        if read < size {
            return body;
        }
        let mut end = String::new();
        let _ = reader.read_line(&mut end);
    }
}

fn server_config() -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let dir = test_ca();
    let certs = CertificateDer::pem_file_iter(dir.join("server.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(dir.join("server-key.pem")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    Arc::new(config)
}

/// A loopback port without a listener: a connection to it is refused, at once on Linux and
/// macOS and after the retries of the connect on Windows (about two seconds). Another test may
/// bind the port in between, which is rare.
pub fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// The bytes of a checked-in example message of the core.
pub fn example(name: &str) -> &'static [u8] {
    stemma_core::protocol::examples::ALL
        .iter()
        .find(|example| example.name == name)
        .unwrap_or_else(|| panic!("no example `{name}`"))
        .bytes
}

/// Removes the proxy variables from the environment of the test process, so that a connection
/// with TLS to the responder does not go through a proxy of the machine. Every test that calls
/// it sets the same state, so tests that run at the same time do not disturb each other.
pub fn without_proxy() {
    for name in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        std::env::remove_var(name);
    }
}
