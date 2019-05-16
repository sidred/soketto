//! Websocket [handshake] codecs.
//!
//! [handshake]: https://tools.ietf.org/html/rfc6455#section-4

use bytes::BytesMut;
use http::StatusCode;
use sha1::Sha1;
use smallvec::SmallVec;
use std::{io, fmt, str};
use tokio_codec::{Decoder, Encoder};
use unicase::Ascii;

// Handshake codec ////////////////////////////////////////////////////////////////////////////////

// Defined in RFC6455 and used to generate the `Sec-WebSocket-Accept` header
// in the server handshake response.
const KEY: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// How many HTTP headers do we support during parsing?
const MAX_NUM_HEADERS: usize = 32;

// Some HTTP headers we need to check during parsing.
const SEC_WEBSOCKET_EXTENSIONS: Ascii<&str> = Ascii::new("Sec-WebSocket-Extensions");
const SEC_WEBSOCKET_PROTOCOL: Ascii<&str> = Ascii::new("Sec-WebSocket-Protocol");

// Handshake client (initiator) ///////////////////////////////////////////////////////////////////

/// Handshake client codec.
#[derive(Debug)]
pub struct Client<'a> {
    secure: bool,
    host: &'a str,
    resource: &'a str,
    origin: Option<&'a str>,
    nonce: &'a str,
    protocols: SmallVec<[&'a str; 4]>,
    extensions: SmallVec<[&'a str; 4]>,
}

impl<'a> Client<'a> {
    pub fn new(host: &'a str, resource: &'a str, nonce: &'a str) -> Self {
        Client {
            secure: true,
            host,
            resource,
            origin: None,
            nonce,
            protocols: SmallVec::new(),
            extensions: SmallVec::new()
        }
    }

    pub fn insecure(&mut self) -> &mut Self {
        self.secure = false;
        self
    }

    pub fn set_origin(&mut self, o: &'a str) -> &mut Self {
        self.origin = Some(o);
        self
    }

    pub fn add_protocol(&mut self, p: &'a str) -> &mut Self {
        self.protocols.push(p);
        self
    }

    pub fn add_extension(&mut self, e: &'a str) -> &mut Self {
        self.extensions.push(e);
        self
    }
}


impl<'a> Encoder for Client<'a> {
    type Item = ();
    type Error = Error;

    // encode client handshake request
    fn encode(&mut self, _: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        buf.extend_from_slice(b"GET ");
        buf.extend_from_slice(self.resource.as_bytes());
        buf.extend_from_slice(b" HTTP/1.1");
        buf.extend_from_slice(b"\r\nHost: ");
        buf.extend_from_slice(self.host.as_bytes());
        buf.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: upgrade");
        buf.extend_from_slice(b"\r\nSec-WebSocket-Key: ");
        buf.extend_from_slice(self.nonce.as_bytes());
        if let Some(o) = self.origin {
            buf.extend_from_slice(b"\r\nOrigin: ");
            buf.extend_from_slice(o.as_bytes())
        }
        if let Some((last, prefix)) = self.protocols.split_last() {
            buf.extend_from_slice(b"\r\nSec-WebSocket-Protocol: ");
            for p in prefix {
                buf.extend_from_slice(p.as_bytes());
                buf.extend_from_slice(b",")
            }
            buf.extend_from_slice(last.as_bytes())
        }
        if let Some((last, prefix)) = self.extensions.split_last() {
            buf.extend_from_slice(b"\r\nSec-WebSocket-Extensions: ");
            for p in prefix {
                buf.extend_from_slice(p.as_bytes());
                buf.extend_from_slice(b",")
            }
            buf.extend_from_slice(last.as_bytes())
        }
        buf.extend_from_slice(b"\r\nSec-WebSocket-Version: 13\r\n\r\n");
        Ok(())
    }
}

/// Server handshake response.
#[derive(Debug)]
pub struct Response<'a> {
    protocol: Option<&'a str>,
    extensions: SmallVec<[&'a str; 4]>
}

impl<'a> Response<'a> {
    pub fn protocol(&self) -> Option<&str> {
        self.protocol
    }

    pub fn extensions(&self) -> &[&str] {
        &self.extensions
    }
}

impl<'a> Decoder for Client<'a> {
    type Item = Response<'a>;
    type Error = Error;

    // decode server handshake response
    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut header_buf = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
        let mut response = httparse::Response::new(&mut header_buf);

        let offset = match response.parse(bytes) {
            Ok(httparse::Status::Complete(off)) => off,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(e) => return Err(Error::Http(Box::new(e)))
        };

        if response.version != Some(1) {
            return Err(Error::UnsupportedHttpVersion)
        }
        if response.code != Some(101) {
            return Err(Error::UnexpectedStatusCode(response.code.unwrap_or(0)))
        }

        expect_header(&response.headers, "Upgrade", "websocket")?;
        expect_header(&response.headers, "Connection", "upgrade")?;

        let nonce = self.nonce;
        with_header(&response.headers, "Sec-WebSocket-Accept", move |theirs| {
            let mut digest = Sha1::new();
            digest.update(nonce.as_bytes());
            digest.update(KEY);
            let ours = base64::encode(&digest.digest().bytes());
            if ours.as_bytes() != theirs {
                return Err(Error::InvalidSecWebSocketAccept)
            }
            Ok(())
        })?;

        // Match `Sec-WebSocket-Extensions` headers.

        let mut selected_exts = SmallVec::new();
        for e in response.headers.iter().filter(|h| Ascii::new(h.name) == SEC_WEBSOCKET_EXTENSIONS) {
            match self.extensions.iter().find(|x| x.as_bytes() == e.value) {
                Some(&x) => selected_exts.push(x),
                None => return Err(Error::UnsolicitedExtension)
            }
        }

        // Match `Sec-WebSocket-Protocol` header.

        let their_proto = response.headers
            .iter()
            .find(|h| Ascii::new(h.name) == SEC_WEBSOCKET_PROTOCOL);

        let mut selected_proto = None;

        if let Some(tp) = their_proto {
            if let Some(&p) = self.protocols.iter().find(|x| x.as_bytes() == tp.value) {
                selected_proto = Some(p)
            } else {
                return Err(Error::UnsolicitedProtocol)
            }
        }

        bytes.split_to(offset); // chop off the HTTP part we have processed

        Ok(Some(Response { protocol: selected_proto, extensions: selected_exts }))
    }
}

// Handshake server (responder) ///////////////////////////////////////////////////////////////////

/// Handshake server codec.
#[derive(Debug)]
pub struct Server<'a> {
    protocols: SmallVec<[&'a str; 4]>,
    extensions: SmallVec<[&'a str; 4]>
}

impl<'a> Server<'a> {
    pub fn new() -> Self {
        Server {
            protocols: SmallVec::new(),
            extensions: SmallVec::new()
        }
    }

    pub fn add_protocol(&mut self, p: &'a str) -> &mut Self {
        self.protocols.push(p);
        self
    }

    pub fn add_extension(&mut self, e: &'a str) -> &mut Self {
        self.extensions.push(e);
        self
    }
}

/// Client handshake request
#[derive(Debug)]
pub struct Request<'a> {
    ws_key: SmallVec<[u8; 32]>,
    protocols: SmallVec<[&'a str; 4]>,
    extensions: SmallVec<[&'a str; 4]>
}

impl<'a> Request<'a> {
    pub fn key(&self) -> &[u8] {
        &self.ws_key
    }

    pub fn protocols(&self) -> &[&str] {
        &self.extensions
    }

    pub fn extensions(&self) -> &[&str] {
        &self.extensions
    }
}

impl<'a> Decoder for Server<'a> {
    type Item = Request<'a>;
    type Error = Error;

    // decode client request
    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut header_buf = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
        let mut request = httparse::Request::new(&mut header_buf);

        let offset = match request.parse(bytes) {
            Ok(httparse::Status::Complete(off)) => off,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(e) => return Err(Error::Http(Box::new(e)))
        };

        if request.method != Some("GET") {
            return Err(Error::InvalidRequestMethod)
        }
        if request.version != Some(1) {
            return Err(Error::UnsupportedHttpVersion)
        }

        // TODO: Host Validation
        with_header(&request.headers, "Host", |_h| Ok(()))?;

        expect_header(&request.headers, "Upgrade", "websocket")?;
        expect_header(&request.headers, "Connection", "upgrade")?;
        expect_header(&request.headers, "Sec-WebSocket-Version", "13")?;

        let ws_key = with_header(&request.headers, "Sec-WebSocket-Key", |k| {
            Ok(SmallVec::from(k))
        })?;

        let mut extensions = SmallVec::new();
        for e in request.headers.iter().filter(|h| Ascii::new(h.name) == SEC_WEBSOCKET_EXTENSIONS) {
            if let Some(&x) = self.extensions.iter().find(|x| x.as_bytes() == e.value) {
                extensions.push(x)
            }
        }

        let mut protocols = SmallVec::new();
        for p in request.headers.iter().filter(|h| Ascii::new(h.name) == SEC_WEBSOCKET_PROTOCOL) {
            if let Some(&x) = self.protocols.iter().find(|x| x.as_bytes() == p.value) {
                protocols.push(x)
            }
        }

        bytes.split_to(offset); // chop off the HTTP part we have processed

        Ok(Some(Request { ws_key, protocols, extensions }))
    }
}

/// Successful handshake response the server wants to send to the client.
#[derive(Debug)]
pub struct Accept<'a> {
    key: &'a [u8],
    protocol: Option<&'a str>,
    extensions: SmallVec<[&'a str; 4]>
}

impl<'a> Accept<'a> {
    pub fn new(key: &'a [u8]) -> Self {
        Accept {
            key,
            protocol: None,
            extensions: SmallVec::new()
        }
    }

    pub fn set_protocol(&mut self, p: &'a str) -> &mut Self {
        self.protocol = Some(p);
        self
    }

    pub fn add_extension(&mut self, e: &'a str) -> &mut Self {
        self.extensions.push(e);
        self
    }
}

/// Error handshake response the server wants to send to the client.
#[derive(Debug)]
pub struct Reject {
    code: u16
}

impl Reject {
    pub fn new(code: u16) -> Self {
        Reject { code }
    }
}

impl<'a> Encoder for Server<'a> {
    type Item = Result<Accept<'a>, Reject>;
    type Error = Error;

    // encode server handshake response
    fn encode(&mut self, answer: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        match answer {
            Ok(accept) => {
                let mut key_buf = [0; 32];
                let accept_value = {
                    let mut digest = Sha1::new();
                    digest.update(accept.key);
                    digest.update(KEY);
                    let d = digest.digest().bytes();
                    let n = base64::encode_config_slice(&d, base64::STANDARD, &mut key_buf);
                    &key_buf[.. n]
                };
                buf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols");
                buf.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: upgrade");
                buf.extend_from_slice(b"\r\nSec-WebSocket-Accept: ");
                buf.extend_from_slice(accept_value);
                if let Some(p) = accept.protocol {
                    buf.extend_from_slice(b"\r\nSec-WebSocket-Protocol: ");
                    buf.extend_from_slice(p.as_bytes())
                }
                if let Some((last, prefix)) = accept.extensions.split_last() {
                    buf.extend_from_slice(b"\r\nSec-WebSocket-Extensions: ");
                    for p in prefix {
                        buf.extend_from_slice(p.as_bytes());
                        buf.extend_from_slice(b",")
                    }
                    buf.extend_from_slice(last.as_bytes())
                }
                buf.extend_from_slice(b"\r\n\n\n")
            }
            Err(reject) => {
                buf.extend_from_slice(b"HTTP/1.1 ");
                let s = StatusCode::from_u16(reject.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                buf.extend_from_slice(s.as_str().as_bytes());
                buf.extend_from_slice(b" ");
                buf.extend_from_slice(s.canonical_reason().unwrap_or("N/A").as_bytes());
                buf.extend_from_slice(b"\r\n\r\n")
            }
        }
        Ok(())
    }
}

fn expect_header(headers: &[httparse::Header], name: &str, ours: &str) -> Result<(), Error> {
    with_header(headers, name, move |theirs| {
        let s = str::from_utf8(theirs)?;
        if Ascii::new(s) == Ascii::new(ours) {
            Ok(())
        } else {
            Err(Error::UnexpectedHeader(name.into()))
        }
    })
}

fn with_header<F, R>(headers: &[httparse::Header], name: &str, f: F) -> Result<R, Error>
where
    F: Fn(&[u8]) -> Result<R, Error>
{
    let ascii_name = Ascii::new(name);
    if let Some(h) = headers.iter().find(move |h| Ascii::new(h.name) == ascii_name) {
        f(h.value)
    } else {
        Err(Error::HeaderNotFound(name.into()))
    }
}

// Codec error type ///////////////////////////////////////////////////////////////////////////////

#[derive(Debug)]
pub enum Error {
    /// An I/O error has been encountered.
    Io(io::Error),
    /// An HTTP version =/= 1.1 was encountered.
    UnsupportedHttpVersion,
    /// The handshake request was not a GET request.
    InvalidRequestMethod,
    /// The HTTP response code was unexpected.
    UnexpectedStatusCode(u16),
    /// An HTTP header has not been present.
    HeaderNotFound(String),
    /// An HTTP header value was not expected.
    UnexpectedHeader(String),
    /// The Sec-WebSocket-Accept header value did not match.
    InvalidSecWebSocketAccept,
    /// The server returned an extension we did not ask for.
    UnsolicitedExtension,
    /// The server returned a protocol we did not ask for.
    UnsolicitedProtocol,
    /// The HTTP entity could not be parsed successfully.
    Http(Box<dyn std::error::Error + Send + 'static>),
    /// UTF-8 decoding failed.
    Utf8(std::str::Utf8Error),

    #[doc(hidden)]
    __Nonexhaustive
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {}", e),
            Error::Http(e) => write!(f, "http parser error: {}", e),
            Error::HeaderNotFound(n) => write!(f, "header {} not found", n),
            Error::UnexpectedHeader(n) => write!(f, "header {} had unexpected value", n),
            Error::Utf8(e) => write!(f, "utf-8 decoding error: {}", e),
            Error::UnexpectedStatusCode(c) => write!(f, "unexpected response status: {}", c),
            Error::UnsupportedHttpVersion => f.write_str("http version was not 1.1"),
            Error::InvalidRequestMethod => f.write_str("handshake not a GET request"),
            Error::InvalidSecWebSocketAccept => f.write_str("websocket key mismatch"),
            Error::UnsolicitedExtension => f.write_str("unsolicited extension returned"),
            Error::UnsolicitedProtocol => f.write_str("unsolicited protocol returned"),
            Error::__Nonexhaustive => f.write_str("__Nonexhaustive")
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Utf8(e) => Some(e),
            Error::Http(e) => Some(&**e),
            Error::HeaderNotFound(_)
            | Error::UnexpectedHeader(_)
            | Error::UnexpectedStatusCode(_)
            | Error::UnsupportedHttpVersion
            | Error::InvalidRequestMethod
            | Error::InvalidSecWebSocketAccept
            | Error::UnsolicitedExtension
            | Error::UnsolicitedProtocol
            | Error::__Nonexhaustive => None
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<str::Utf8Error> for Error {
    fn from(e: str::Utf8Error) -> Self {
        Error::Utf8(e)
    }
}