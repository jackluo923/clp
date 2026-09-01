//! Bounded streaming HTTP(S) source adapters and S3 query authentication.
//!
//! The adapter accepts only the transport surface documented by `clp-s`: ordinary HTTP(S) GETs
//! and HTTP(S) S3 object URLs authenticated with AWS Signature Version 4. The pinned C++ binary
//! sends every nonexistent filesystem spelling to its particular libcurl build, which incidentally
//! makes `file://`, FTP, mail, and several other schemes usable. Those schemes are deliberately
//! rejected here rather than becoming an unstable library contract. `s3://` is also rejected: the
//! C++ signer and its libcurl build both require an HTTP(S) S3 URL.
//!
//! [`HttpReader`] never buffers a complete response. It preserves the exact caller-provided URL as
//! the source name while keeping a generated S3 query URL private. [`ForwardSeekReader`] adds only
//! forward seeking by consuming bytes, matching the one-pass access supported by the C++ network
//! reader without a memory or temporary-file copy of the complete source.

use std::error::Error;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use hmac::Hmac;
use hmac::Mac;
use reqwest::Certificate;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::blocking::Response;
use reqwest::redirect::Policy;
use sha2::Digest;
use sha2::Sha256;
use time::OffsetDateTime;

const GIBIBYTE: u64 = 1024 * 1024 * 1024;
const DEFAULT_S3_REGION: &str = "us-east-1";
const S3_SERVICE: &str = "s3";
const AWS4_REQUEST: &str = "aws4_request";
const AWS4_ALGORITHM: &str = "AWS4-HMAC-SHA256";
const S3_EXPIRATION_SECONDS: u64 = 86_400;
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const SIGNED_HEADERS: &str = "host";
const SEEK_SCRATCH_BYTES: usize = 64 * 1024;

/// Hard limits applied independently to one HTTP response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkLimits {
    response_bytes: u64,
}

impl NetworkLimits {
    /// Safety-oriented default suitable for large input objects.
    pub const DEFAULT: Self = Self::new(64 * GIBIBYTE);

    /// Creates a response-body limit.
    #[must_use]
    pub const fn new(max_response_bytes: u64) -> Self {
        Self {
            response_bytes: max_response_bytes,
        }
    }

    /// Returns the maximum response-body bytes that may be read.
    #[must_use]
    pub const fn max_response_bytes(self) -> u64 {
        self.response_bytes
    }
}

impl Default for NetworkLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Configuration shared by all requests from one [`HttpClient`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpClientOptions {
    limits: NetworkLimits,
    connect_timeout: Option<Duration>,
    request_timeout: Option<Duration>,
}

impl HttpClientOptions {
    /// Safety-oriented response limits with the C++ compatibility policy of no implicit timeout.
    pub const DEFAULT: Self = Self::new(NetworkLimits::DEFAULT);

    /// Creates options with explicit limits and no connection or whole-request timeout.
    #[must_use]
    pub const fn new(limits: NetworkLimits) -> Self {
        Self {
            limits,
            connect_timeout: None,
            request_timeout: None,
        }
    }

    /// Creates C++-compatible unbounded response options.
    ///
    /// Bindings and services should normally prefer an explicit finite limit.
    #[must_use]
    pub const fn compatibility_unbounded() -> Self {
        Self::new(NetworkLimits::new(u64::MAX))
    }

    /// Sets or disables the connection timeout.
    #[must_use]
    pub const fn with_connect_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets or disables the timeout covering response headers and body reads.
    #[must_use]
    pub const fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Returns the response limits.
    #[must_use]
    pub const fn limits(self) -> NetworkLimits {
        self.limits
    }

    /// Returns the configured connection timeout.
    #[must_use]
    pub const fn connect_timeout(self) -> Option<Duration> {
        self.connect_timeout
    }

    /// Returns the configured whole-request timeout.
    #[must_use]
    pub const fn request_timeout(self) -> Option<Duration> {
        self.request_timeout
    }
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Borrowed S3 credentials used only while generating one query-authenticated request URL.
#[derive(Clone, Copy)]
pub struct S3CredentialsRef<'credentials> {
    access_key_id: &'credentials str,
    secret_access_key: &'credentials str,
    session_token: Option<&'credentials str>,
}

impl<'credentials> S3CredentialsRef<'credentials> {
    /// Creates borrowed credentials. Empty values are passed through exactly as C++ does.
    #[must_use]
    pub const fn new(
        access_key_id: &'credentials str,
        secret_access_key: &'credentials str,
        session_token: Option<&'credentials str>,
    ) -> Self {
        Self {
            access_key_id,
            secret_access_key,
            session_token,
        }
    }
}

impl Debug for S3CredentialsRef<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3CredentialsRef")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field("session_token_present", &self.session_token.is_some())
            .finish()
    }
}

/// Authentication applied to an HTTP(S) input request.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum NetworkAuth<'credentials> {
    /// Send the caller's URL without adding authentication.
    None,
    /// Replace any original query with an AWS Signature Version 4 query for an S3 GET.
    S3(S3CredentialsRef<'credentials>),
}

impl Debug for NetworkAuth<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::S3(credentials) => formatter.debug_tuple("S3").field(credentials).finish(),
        }
    }
}

/// Stable classification of a sanitized HTTP transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpTransportErrorKind {
    /// A connection could not be established.
    Connect,
    /// A configured timeout elapsed.
    Timeout,
    /// Request construction, TLS, proxy, protocol, or another transport operation failed.
    Request,
    /// Reading the response body failed after headers were received.
    Body,
}

impl Display for HttpTransportErrorKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connect => "connection failed",
            Self::Timeout => "request timed out",
            Self::Request => "request failed",
            Self::Body => "response body read failed",
        })
    }
}

/// Failure to initialize a reusable HTTP client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpClientBuildError {
    kind: HttpTransportErrorKind,
}

impl HttpClientBuildError {
    /// Returns the stable, credential-free failure classification.
    #[must_use]
    pub const fn kind(self) -> HttpTransportErrorKind {
        self.kind
    }
}

impl Display for HttpClientBuildError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to initialize HTTP client: {}", self.kind)
    }
}

impl Error for HttpClientBuildError {}

/// S3 URL validation or signing failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum S3PresignError {
    /// The input was not an absolute URL.
    InvalidUrl,
    /// Only HTTP and HTTPS S3 URLs are supported.
    UnsupportedScheme,
    /// The URL contained a fragment, user information, or no valid host.
    InvalidAuthority,
    /// Neither a documented virtual-hosted nor path-style S3 object URL matched.
    InvalidS3Url,
    /// The requested signing time predates the Unix epoch.
    TimeBeforeUnixEpoch,
    /// The requested signing time cannot be represented by the timestamp formatter.
    TimeOutOfRange,
    /// HMAC initialization unexpectedly failed.
    SigningFailed,
}

impl Display for S3PresignError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUrl => "invalid absolute S3 URL",
            Self::UnsupportedScheme => "S3 authentication requires an HTTP(S) URL",
            Self::InvalidAuthority => "invalid S3 URL authority",
            Self::InvalidS3Url => "invalid virtual-hosted or path-style S3 object URL",
            Self::TimeBeforeUnixEpoch => "S3 signing time predates the Unix epoch",
            Self::TimeOutOfRange => "S3 signing time is out of range",
            Self::SigningFailed => "S3 request signing failed",
        })
    }
}

impl Error for S3PresignError {}

/// Failure to open one HTTP response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpOpenError {
    /// The source was not an absolute URL.
    InvalidUrl,
    /// The absolute URL did not use HTTP or HTTPS.
    UnsupportedScheme,
    /// S3 URL validation or signing failed.
    Authentication(S3PresignError),
    /// Sending the request failed. The generated authenticated URL is intentionally not retained.
    Transport(HttpTransportErrorKind),
    /// The server returned an HTTP client or server error.
    Status {
        /// Numeric HTTP response status.
        status: u16,
    },
    /// A declared response length exceeded the configured limit before body reads began.
    LimitExceeded(HttpLimitViolation),
}

impl Display for HttpOpenError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => formatter.write_str("invalid absolute input URL"),
            Self::UnsupportedScheme => {
                formatter.write_str("input URL scheme is unsupported; expected HTTP or HTTPS")
            }
            Self::Authentication(error) => {
                write!(formatter, "failed to authenticate input: {error}")
            }
            Self::Transport(kind) => write!(formatter, "failed to open HTTP input: {kind}"),
            Self::Status { status } => write!(formatter, "HTTP input returned status {status}"),
            Self::LimitExceeded(violation) => Display::fmt(violation, formatter),
        }
    }
}

impl Error for HttpOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Authentication(error) => Some(error),
            Self::LimitExceeded(violation) => Some(violation),
            Self::InvalidUrl
            | Self::UnsupportedScheme
            | Self::Transport(_)
            | Self::Status { .. } => None,
        }
    }
}

/// Exact response-byte limit violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimitViolation {
    actual: u64,
    limit: u64,
}

impl HttpLimitViolation {
    const fn new(actual: u64, limit: u64) -> Self {
        Self { actual, limit }
    }

    /// Returns the declared or minimum observed response size.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }

    /// Returns the configured maximum response size.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

impl Display for HttpLimitViolation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HTTP response size {} exceeds limit {}",
            self.actual, self.limit
        )
    }
}

impl Error for HttpLimitViolation {}

/// Failure while streaming an opened response body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpReadError {
    /// The response transport failed after headers were received.
    Transport(HttpTransportErrorKind),
    /// The streamed response exceeded the configured boundary.
    LimitExceeded(HttpLimitViolation),
}

impl Display for HttpReadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(kind) => write!(formatter, "failed to read HTTP input: {kind}"),
            Self::LimitExceeded(violation) => Display::fmt(violation, formatter),
        }
    }
}

impl Error for HttpReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LimitExceeded(violation) => Some(violation),
            Self::Transport(_) => None,
        }
    }
}

/// Reusable blocking HTTP(S) client with fixed redirect, timeout, proxy, and limit policy.
#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    options: HttpClientOptions,
}

impl Debug for HttpClient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Initializes a reusable client. Redirect following and content decoding are disabled.
    ///
    /// # Errors
    ///
    /// Returns a sanitized initialization error if TLS or client setup fails.
    pub fn new(options: HttpClientOptions) -> Result<Self, HttpClientBuildError> {
        Self::build(options, None)
    }

    /// Initializes a reusable client that trusts only a caller-provided PEM CA bundle.
    ///
    /// This explicit byte-oriented entry point lets a CLI reproduce C++'s `CURL_CA_BUNDLE`
    /// precedence without making the library read process-global environment variables or paths.
    /// When no explicit bundle is supplied, [`Self::new`] retains the platform verifier, including
    /// its ordinary `SSL_CERT_FILE` handling.
    ///
    /// # Errors
    ///
    /// Returns a sanitized initialization error when the PEM bundle or TLS client is invalid.
    pub fn new_with_ca_bundle(
        options: HttpClientOptions,
        pem_bundle: &[u8],
    ) -> Result<Self, HttpClientBuildError> {
        let certificates =
            Certificate::from_pem_bundle(pem_bundle).map_err(|error| HttpClientBuildError {
                kind: classify_request_error(&error),
            })?;
        if certificates.is_empty() {
            return Err(HttpClientBuildError {
                kind: HttpTransportErrorKind::Request,
            });
        }
        Self::build(options, Some(certificates))
    }

    fn build(
        options: HttpClientOptions,
        certificates: Option<Vec<Certificate>>,
    ) -> Result<Self, HttpClientBuildError> {
        let mut builder = Client::builder().redirect(Policy::none());
        if let Some(certificates) = certificates {
            builder = builder.tls_certs_only(certificates);
        }
        if let Some(timeout) = options.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        if let Some(timeout) = options.request_timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder.build().map_err(|error| HttpClientBuildError {
            kind: classify_request_error(&error),
        })?;
        Ok(Self { client, options })
    }

    /// Returns this client's immutable request policy.
    #[must_use]
    pub const fn options(&self) -> HttpClientOptions {
        self.options
    }

    /// Opens one streaming HTTP(S) GET while retaining the exact original URL as its source name.
    ///
    /// S3 authentication discards an existing query exactly as the C++ signer does. The generated
    /// query URL is used only for the request and is never exposed by the returned reader or error.
    ///
    /// # Errors
    ///
    /// Returns a typed error for invalid or unsupported URLs, S3 validation/signing, sanitized
    /// transport failure, HTTP 4xx/5xx status, or an over-limit declared body length.
    pub fn open(
        &self,
        source_name: &str,
        auth: NetworkAuth<'_>,
    ) -> Result<HttpReader, HttpOpenError> {
        self.open_at(source_name, auth, SystemTime::now())
    }

    fn open_at(
        &self,
        source_name: &str,
        auth: NetworkAuth<'_>,
        signing_time: SystemTime,
    ) -> Result<HttpReader, HttpOpenError> {
        let request_url = match auth {
            NetworkAuth::None => parse_http_url(source_name)?.to_string(),
            NetworkAuth::S3(credentials) => {
                presign_s3_get_at(source_name, credentials, signing_time)
                    .map_err(HttpOpenError::Authentication)?
            }
        };

        let response = self
            .client
            .get(request_url)
            .send()
            .map_err(|error| HttpOpenError::Transport(classify_request_error(&error)))?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            return Err(HttpOpenError::Status {
                status: status.as_u16(),
            });
        }
        let content_length = response.content_length();
        if let Some(actual) = content_length {
            let limit = self.options.limits.max_response_bytes();
            if actual > limit {
                return Err(HttpOpenError::LimitExceeded(HttpLimitViolation::new(
                    actual, limit,
                )));
            }
        }

        Ok(HttpReader {
            response,
            source_name: source_name.to_owned(),
            content_length,
            max_bytes: self.options.limits.max_response_bytes(),
            bytes_read: 0,
            eof: false,
            terminal_error: None,
        })
    }
}

/// Streaming response body returned by [`HttpClient::open`].
pub struct HttpReader {
    response: Response,
    source_name: String,
    content_length: Option<u64>,
    max_bytes: u64,
    bytes_read: u64,
    eof: bool,
    terminal_error: Option<HttpReadError>,
}

impl Debug for HttpReader {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpReader")
            .field("source_name", &"[REDACTED]")
            .field("content_length", &self.content_length)
            .field("max_bytes", &self.max_bytes)
            .field("bytes_read", &self.bytes_read)
            .field("eof", &self.eof)
            .field("terminal_error", &self.terminal_error)
            .finish_non_exhaustive()
    }
}

impl HttpReader {
    /// Returns the exact URL spelling supplied by the caller, including its original query.
    #[must_use]
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Returns the server-declared response size, if one was available.
    #[must_use]
    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Returns response bytes successfully delivered to the caller.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Returns whether a nonempty read has observed a clean response-body EOF.
    #[must_use]
    pub const fn is_eof(&self) -> bool {
        self.eof
    }

    /// Reads response bytes while preserving typed limit and transport failures.
    ///
    /// As with every [`Read`] implementation, an empty destination returns zero without probing
    /// EOF. When a streamed response reaches the configured limit exactly, callers must perform a
    /// subsequent nonempty read to distinguish exact EOF from an over-limit next byte.
    ///
    /// # Errors
    ///
    /// Returns a sanitized body transport error or a response-byte limit violation. A terminal
    /// error is sticky so a caller cannot bypass the boundary by retrying.
    pub fn read_typed(&mut self, destination: &mut [u8]) -> Result<usize, HttpReadError> {
        if destination.is_empty() {
            return Ok(0);
        }
        if let Some(error) = self.terminal_error {
            return Err(error);
        }
        if self.eof {
            return Ok(0);
        }

        if self.bytes_read == self.max_bytes {
            let mut probe = [0_u8; 1];
            match self.response.read(&mut probe) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(0);
                }
                Ok(_) => {
                    let actual = self.max_bytes.saturating_add(1);
                    let error = HttpReadError::LimitExceeded(HttpLimitViolation::new(
                        actual,
                        self.max_bytes,
                    ));
                    self.terminal_error = Some(error);
                    return Err(error);
                }
                Err(source) => {
                    let error = HttpReadError::Transport(classify_body_error(&source));
                    self.terminal_error = Some(error);
                    return Err(error);
                }
            }
        }

        let remaining = self.max_bytes - self.bytes_read;
        let bounded_len = destination.len().min(u64_to_usize_saturating(remaining));
        match self.response.read(&mut destination[..bounded_len]) {
            Ok(0) => {
                self.eof = true;
                Ok(0)
            }
            Ok(read) => {
                let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
                self.bytes_read = self.bytes_read.saturating_add(read_u64);
                Ok(read)
            }
            Err(source) => {
                let error = HttpReadError::Transport(classify_body_error(&source));
                self.terminal_error = Some(error);
                Err(error)
            }
        }
    }
}

impl Read for HttpReader {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        self.read_typed(destination).map_err(io::Error::other)
    }
}

/// Failure from a seek operation that a one-pass source cannot satisfy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ForwardSeekError {
    /// Seeking relative to an unknown end requires complete-source buffering.
    EndRelative,
    /// The requested absolute position was behind the current stream position.
    Backward {
        /// Current absolute byte position.
        current: u64,
        /// Requested absolute byte position.
        requested: u64,
    },
    /// A signed or unsigned seek calculation overflowed its valid range.
    PositionOverflow,
    /// The source ended before the requested forward position.
    BeyondEnd {
        /// Requested absolute byte position.
        requested: u64,
        /// First unavailable absolute byte position.
        end: u64,
    },
}

impl Display for ForwardSeekError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndRelative => {
                formatter.write_str("cannot seek relative to an unknown stream end")
            }
            Self::Backward { current, requested } => write!(
                formatter,
                "cannot seek backward from stream position {current} to {requested}"
            ),
            Self::PositionOverflow => formatter.write_str("stream seek position overflow"),
            Self::BeyondEnd { requested, end } => write!(
                formatter,
                "cannot seek to stream position {requested}; source ended at {end}"
            ),
        }
    }
}

impl Error for ForwardSeekError {}

/// Adds current/forward [`Seek`] support to a one-pass [`Read`] source by discarding skipped bytes.
///
/// No source bytes are retained. Backward and end-relative seeks return an `Unsupported` I/O error
/// containing a [`ForwardSeekError`].
pub struct ForwardSeekReader<R> {
    inner: R,
    position: u64,
    scratch: Box<[u8]>,
}

impl<R: Debug> Debug for ForwardSeekReader<R> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForwardSeekReader")
            .field("inner", &self.inner)
            .field("position", &self.position)
            .field("scratch_bytes", &self.scratch.len())
            .finish()
    }
}

impl<R> ForwardSeekReader<R> {
    /// Wraps a source positioned at byte zero.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            position: 0,
            scratch: vec![0; SEEK_SCRATCH_BYTES].into_boxed_slice(),
        }
    }

    /// Returns the current absolute source position.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Returns a shared reference to the underlying source.
    #[must_use]
    pub const fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Removes the adapter and returns the underlying source.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for ForwardSeekReader<R> {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(destination)?;
        let read_u64 = u64::try_from(read).map_err(|_| position_overflow_io())?;
        self.position = self
            .position
            .checked_add(read_u64)
            .ok_or_else(position_overflow_io)?;
        Ok(read)
    }
}

impl<R: Read> Seek for ForwardSeekReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let requested = match position {
            SeekFrom::Start(position) => position,
            SeekFrom::Current(delta) => offset_position(self.position, delta)?,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    ForwardSeekError::EndRelative,
                ));
            }
        };
        if requested < self.position {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                ForwardSeekError::Backward {
                    current: self.position,
                    requested,
                },
            ));
        }

        while self.position < requested {
            let remaining = requested - self.position;
            let chunk_len = self.scratch.len().min(u64_to_usize_saturating(remaining));
            let read = self.inner.read(&mut self.scratch[..chunk_len])?;
            if 0 == read {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    ForwardSeekError::BeyondEnd {
                        requested,
                        end: self.position,
                    },
                ));
            }
            let read_u64 = u64::try_from(read).map_err(|_| position_overflow_io())?;
            self.position = self
                .position
                .checked_add(read_u64)
                .ok_or_else(position_overflow_io)?;
        }
        Ok(self.position)
    }
}

/// Generates the C++-compatible AWS Signature Version 4 query URL for an S3 GET.
///
/// The input must use one of the documented HTTP(S) path-style or virtual-hosted S3 object forms.
/// Any original query is discarded. The timestamp is truncated to whole UTC seconds, expiration is
/// 86,400 seconds, the payload is unsigned, and only `host` is signed.
///
/// # Errors
///
/// Returns a typed error for invalid URL/S3 structure, unsupported schemes or authority fields,
/// an unrepresentable timestamp, or signing initialization failure.
pub fn presign_s3_get_at(
    source_url: &str,
    credentials: S3CredentialsRef<'_>,
    signing_time: SystemTime,
) -> Result<String, S3PresignError> {
    let parsed = ParsedS3Url::parse(source_url)?;
    let (date, timestamp) = format_signing_time(signing_time)?;
    let scope = format!("{date}/{}/{S3_SERVICE}/{AWS4_REQUEST}", parsed.region);
    let credential = format!("{}/{scope}", credentials.access_key_id);
    let mut canonical_query = format!(
        "X-Amz-Algorithm={AWS4_ALGORITHM}&X-Amz-Credential={}&X-Amz-Date={timestamp}&\
         X-Amz-Expires={S3_EXPIRATION_SECONDS}",
        aws_percent_encode(credential.as_bytes(), false)
    );
    if let Some(session_token) = credentials.session_token {
        canonical_query.push_str("&X-Amz-Security-Token=");
        canonical_query.push_str(&aws_percent_encode(session_token.as_bytes(), false));
    }
    canonical_query.push_str("&X-Amz-SignedHeaders=host");

    let canonical_request = format!(
        "GET\n{}\n{canonical_query}\nhost:{}\n\n{SIGNED_HEADERS}\n{UNSIGNED_PAYLOAD}",
        aws_percent_encode(parsed.path.as_bytes(), true),
        parsed.host
    );
    let canonical_hash = Sha256::digest(canonical_request.as_bytes());
    let string_to_sign = format!(
        "{AWS4_ALGORITHM}\n{timestamp}\n{scope}\n{}",
        lower_hex(canonical_hash.as_slice())
    );

    let date_key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    )?;
    let region_key = hmac_sha256(&date_key, parsed.region.as_bytes())?;
    let service_key = hmac_sha256(&region_key, S3_SERVICE.as_bytes())?;
    let signing_key = hmac_sha256(&service_key, AWS4_REQUEST.as_bytes())?;
    let signature = hmac_sha256(&signing_key, string_to_sign.as_bytes())?;

    Ok(format!(
        "{}://{}{}?{canonical_query}&X-Amz-Signature={}",
        parsed.scheme,
        parsed.host,
        parsed.path,
        lower_hex(&signature)
    ))
}

#[derive(Debug)]
struct ParsedS3Url {
    scheme: String,
    host: String,
    region: String,
    path: String,
}

impl ParsedS3Url {
    fn parse(source_url: &str) -> Result<Self, S3PresignError> {
        let url = Url::parse(source_url).map_err(|_| S3PresignError::InvalidUrl)?;
        let (raw_scheme, raw_authority, raw_path) = raw_http_url_parts(source_url)?;
        if !matches!(raw_scheme, "http" | "https") {
            return Err(S3PresignError::UnsupportedScheme);
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(S3PresignError::InvalidAuthority);
        }
        let host_name = url.host_str().ok_or(S3PresignError::InvalidAuthority)?;
        let raw_host = raw_authority
            .rsplit_once(':')
            .map_or(raw_authority, |(host, port)| {
                if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) {
                    host
                } else {
                    raw_authority
                }
            });
        if raw_host != host_name || !valid_endpoint(host_name) {
            return Err(S3PresignError::InvalidAuthority);
        }
        let key_or_path = raw_path
            .strip_prefix('/')
            .ok_or(S3PresignError::InvalidS3Url)?;

        if let Some((bucket, host_tail)) = host_name.rsplit_once(".s3.") {
            if !valid_bucket(bucket) || key_or_path.is_empty() || host_tail.is_empty() {
                return Err(S3PresignError::InvalidS3Url);
            }
            let region = virtual_host_region(host_tail);
            return Ok(Self {
                scheme: raw_scheme.to_owned(),
                host: raw_authority.to_owned(),
                region: region.to_owned(),
                path: format!("/{key_or_path}"),
            });
        }

        let (bucket, key) = key_or_path
            .split_once('/')
            .ok_or(S3PresignError::InvalidS3Url)?;
        if !valid_bucket(bucket) || key.is_empty() {
            return Err(S3PresignError::InvalidS3Url);
        }
        let region = path_style_region(host_name);
        Ok(Self {
            scheme: raw_scheme.to_owned(),
            host: raw_authority.to_owned(),
            region: region.to_owned(),
            path: format!("/{bucket}/{key}"),
        })
    }
}

fn raw_http_url_parts(source_url: &str) -> Result<(&str, &str, &str), S3PresignError> {
    let (scheme, remainder) = source_url
        .split_once("://")
        .ok_or(S3PresignError::InvalidUrl)?;
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(S3PresignError::InvalidAuthority);
    }
    let after_authority = &remainder[authority_end..];
    let path = if after_authority.starts_with('/') {
        let path_end = after_authority
            .find(['?', '#'])
            .unwrap_or(after_authority.len());
        &after_authority[..path_end]
    } else {
        "/"
    };
    Ok((scheme, authority, path))
}

fn parse_http_url(source_url: &str) -> Result<Url, HttpOpenError> {
    let url = Url::parse(source_url).map_err(|_| HttpOpenError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(HttpOpenError::UnsupportedScheme);
    }
    if url.host().is_none() {
        return Err(HttpOpenError::InvalidUrl);
    }
    Ok(url)
}

fn classify_request_error(error: &reqwest::Error) -> HttpTransportErrorKind {
    if error.is_timeout() {
        HttpTransportErrorKind::Timeout
    } else if error.is_connect() {
        HttpTransportErrorKind::Connect
    } else {
        HttpTransportErrorKind::Request
    }
}

fn classify_body_error(error: &io::Error) -> HttpTransportErrorKind {
    if io::ErrorKind::TimedOut == error.kind() {
        HttpTransportErrorKind::Timeout
    } else {
        HttpTransportErrorKind::Body
    }
}

fn format_signing_time(signing_time: SystemTime) -> Result<(String, String), S3PresignError> {
    let duration = signing_time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| S3PresignError::TimeBeforeUnixEpoch)?;
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| S3PresignError::TimeOutOfRange)?;
    let timestamp =
        OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| S3PresignError::TimeOutOfRange)?;
    let month = u8::from(timestamp.month());
    let date = format!("{:04}{month:02}{:02}", timestamp.year(), timestamp.day());
    let complete = format!(
        "{date}T{:02}{:02}{:02}Z",
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    );
    Ok((date, complete))
}

fn virtual_host_region(host_tail: &str) -> &str {
    if "amazonaws.com" == host_tail {
        return DEFAULT_S3_REGION;
    }
    host_tail
        .split_once('.')
        .map_or(DEFAULT_S3_REGION, |(region, _)| region)
}

fn path_style_region(host_name: &str) -> &str {
    let Some(s3_tail) = host_name.strip_prefix("s3.") else {
        return DEFAULT_S3_REGION;
    };
    if "amazonaws.com" == s3_tail {
        return DEFAULT_S3_REGION;
    }
    s3_tail
        .split_once('.')
        .map_or(DEFAULT_S3_REGION, |(region, _)| region)
}

fn valid_bucket(bucket: &str) -> bool {
    !bucket.is_empty()
        && bucket
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
}

fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
}

fn aws_percent_encode(input: &[u8], preserve_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(input.len());
    for &byte in input {
        if byte.is_ascii_alphanumeric()
            || b"-_.~".contains(&byte)
            || (preserve_slash && b'/' == byte)
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn hmac_sha256(key: &[u8], input: &[u8]) -> Result<[u8; 32], S3PresignError> {
    let mut hmac =
        Hmac::<Sha256>::new_from_slice(key).map_err(|_| S3PresignError::SigningFailed)?;
    hmac.update(input);
    let digest = hmac.finalize().into_bytes();
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    Ok(output)
}

fn lower_hex(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(input.len().saturating_mul(2));
    for &byte in input {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn offset_position(position: u64, delta: i64) -> io::Result<u64> {
    if delta >= 0 {
        position
            .checked_add(delta.unsigned_abs())
            .ok_or_else(position_overflow_io)
    } else {
        position
            .checked_sub(delta.unsigned_abs())
            .ok_or_else(position_overflow_io)
    }
}

fn position_overflow_io() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        ForwardSeekError::PositionOverflow,
    )
}

fn u64_to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Write;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::thread::JoinHandle;

    use super::*;
    use crate::ExtractionOptions;
    use crate::archive::SingleFileArchiveReader;
    use crate::extract_jsonl;

    const CPP_SFA: &[u8] = include_bytes!("../tests/fixtures/sfa-v0.5.0-minimal-cpp.bin");

    type Responder = dyn Fn(&str) -> Vec<u8> + Send + Sync;

    struct MockServer {
        address: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responder: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock HTTP server");
            listener
                .set_nonblocking(true)
                .expect("make mock listener nonblocking");
            let address = listener.local_addr().expect("read mock address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_for_thread = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_for_thread = Arc::clone(&stop);
            let responder: Arc<Responder> = Arc::new(responder);
            let thread = thread::spawn(move || {
                while !stop_for_thread.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_request(&mut stream);
                            requests_for_thread
                                .lock()
                                .expect("lock mock requests")
                                .push(request.clone());
                            let response = responder(&request);
                            stream.write_all(&response).expect("write mock response");
                        }
                        Err(error) if io::ErrorKind::WouldBlock == error.kind() => {
                            thread::yield_now();
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                }
            });
            Self {
                address,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.address)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("lock mock requests").clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("join mock server");
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set mock read timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| b"\r\n\r\n" == window) {
            let read = stream.read(&mut buffer).expect("read mock request");
            assert!(0 < read, "request headers ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(request).expect("request is ASCII")
    }

    fn content_length_response(status: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn chunked_response(chunks: &[&[u8]]) -> Vec<u8> {
        let mut response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for chunk in chunks {
            write!(response, "{:x}\r\n", chunk.len()).expect("format chunk size");
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    }

    fn client(max_response_bytes: u64) -> HttpClient {
        HttpClient::new(HttpClientOptions::new(NetworkLimits::new(
            max_response_bytes,
        )))
        .expect("initialize test HTTP client")
    }

    #[test]
    fn explicit_ca_bundle_failure_is_sanitized_and_does_not_fall_back_to_platform_roots() {
        let error = HttpClient::new_with_ca_bundle(HttpClientOptions::default(), b"not a PEM CA")
            .expect_err("invalid explicit CA bundle must fail");
        assert_eq!(HttpTransportErrorKind::Request, error.kind());
        assert!(!format!("{error:?} {error}").contains("not a PEM CA"));
    }

    #[test]
    fn streams_chunked_body_and_preserves_exact_source_name() {
        let server = MockServer::start(|_| chunked_response(&[b"abc", b"defg", b"h"]));
        let source = format!("{}?token=caller-value#fragment", server.url("/input"));
        let mut reader = client(8)
            .open(&source, NetworkAuth::None)
            .expect("open chunked body");

        assert_eq!(source, reader.source_name());
        assert_eq!(None, reader.content_length());
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("stream complete body");
        assert_eq!(b"abcdefgh", body.as_slice());
        assert_eq!(8, reader.bytes_read());
        assert!(reader.is_eof());

        let requests = server.requests();
        assert_eq!(1, requests.len());
        assert!(requests[0].starts_with("GET /input?token=caller-value HTTP/1.1\r\n"));
        assert!(!requests[0].contains("#fragment"));
    }

    #[test]
    fn leaves_content_encoding_bytes_for_decoded_input() {
        let compressed = [0x1f, 0x8b, 0x08, 0x00, 0xaa, 0xbb, 0xcc];
        let server = MockServer::start(move |_| {
            content_length_response("200 OK", &compressed, "Content-Encoding: gzip\r\n")
        });
        let mut reader = client(32)
            .open(&server.url("/wrapped.json"), NetworkAuth::None)
            .expect("open encoded response");
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .expect("read physical response bytes");

        assert_eq!(compressed.as_slice(), body.as_slice());
    }

    #[test]
    fn does_not_follow_redirects_like_cpp() {
        let server = MockServer::start(|request| {
            if request.starts_with("GET /redirect ") {
                content_length_response("302 Found", b"", "Location: /target\r\n")
            } else {
                content_length_response("200 OK", b"unexpected", "")
            }
        });
        let mut reader = client(10)
            .open(&server.url("/redirect"), NetworkAuth::None)
            .expect("302 is not an HTTP error");

        let mut output = Vec::new();
        reader
            .read_to_end(&mut output)
            .expect("read empty 302 body");
        assert_eq!(&[] as &[u8], output.as_slice());
        assert_eq!(1, server.requests().len());
    }

    #[test]
    fn rejects_http_errors_and_declared_over_limit_bodies_before_reading() {
        let not_found = MockServer::start(|_| content_length_response("404 Not Found", b"no", ""));
        assert_eq!(
            HttpOpenError::Status { status: 404 },
            client(10)
                .open(&not_found.url("/missing"), NetworkAuth::None)
                .expect_err("404 must fail")
        );

        let too_large = MockServer::start(|_| content_length_response("200 OK", b"12345", ""));
        assert_eq!(
            HttpOpenError::LimitExceeded(HttpLimitViolation::new(5, 4)),
            client(4)
                .open(&too_large.url("/large"), NetworkAuth::None)
                .expect_err("declared over-limit body must fail at open")
        );
    }

    #[test]
    fn exact_streaming_limit_requires_eof_probe_and_cannot_be_bypassed() {
        let exact = MockServer::start(|_| chunked_response(&[b"1234"]));
        let mut exact_reader = client(4)
            .open(&exact.url("/exact"), NetworkAuth::None)
            .expect("open exact body");
        let mut buffer = [0_u8; 8];
        assert_eq!(4, exact_reader.read_typed(&mut buffer).expect("read limit"));
        assert!(!exact_reader.is_eof());
        assert_eq!(0, exact_reader.read_typed(&mut buffer).expect("probe EOF"));
        assert!(exact_reader.is_eof());

        let over = MockServer::start(|_| chunked_response(&[b"1234", b"5"]));
        let mut over_reader = client(4)
            .open(&over.url("/over"), NetworkAuth::None)
            .expect("open chunked body");
        assert_eq!(4, over_reader.read_typed(&mut buffer).expect("read limit"));
        let expected = HttpReadError::LimitExceeded(HttpLimitViolation::new(5, 4));
        assert_eq!(
            expected,
            over_reader
                .read_typed(&mut buffer)
                .expect_err("next byte exceeds limit")
        );
        assert_eq!(
            expected,
            over_reader
                .read_typed(&mut buffer)
                .expect_err("limit error is sticky")
        );
    }

    #[test]
    fn explicitly_rejects_incidental_cpp_curl_schemes() {
        for source in [
            "s3://bucket/key",
            "file:///tmp/input.json",
            "ftp://example.com/input.json",
        ] {
            assert_eq!(
                HttpOpenError::UnsupportedScheme,
                client(1)
                    .open(source, NetworkAuth::None)
                    .expect_err("undocumented scheme must fail")
            );
        }
        assert_eq!(
            HttpOpenError::InvalidUrl,
            client(1)
                .open("missing-local-path", NetworkAuth::None)
                .expect_err("relative path is not a URL")
        );
    }

    #[test]
    fn s3_signature_matches_captured_cpp_path_style_request() {
        let credentials = S3CredentialsRef::new("AKIDEXAMPLE", "secret", Some("token+/="));
        let signing_time = UNIX_EPOCH + Duration::from_secs(1_788_111_254);
        let signed = presign_s3_get_at(
            "http://127.0.0.1:18765/bucket/plain.json?discard=yes",
            credentials,
            signing_time,
        )
        .expect("sign path-style URL");

        assert_eq!(
            concat!(
                "http://127.0.0.1:18765/bucket/plain.json?",
                "X-Amz-Algorithm=AWS4-HMAC-SHA256&",
                "X-Amz-Credential=AKIDEXAMPLE%2F20260830%2Fus-east-1%2Fs3%2Faws4_request&",
                "X-Amz-Date=20260830T173414Z&X-Amz-Expires=86400&",
                "X-Amz-Security-Token=token%2B%2F%3D&X-Amz-SignedHeaders=host&",
                "X-Amz-Signature=",
                "d83e8805904a710eda62fb60ab0023d1af7545774aad5388067c9deb6e3c6a53",
            ),
            signed
        );
        assert!(!signed.contains("discard=yes"));
    }

    #[test]
    fn authenticated_open_uses_signed_query_but_retains_only_original_source_name() {
        let server = MockServer::start(|_| content_length_response("200 OK", b"{}", ""));
        let original = server.url("/bucket/input.json?discard=this-query");
        let credentials = S3CredentialsRef::new("access", "secret", Some("session+/="));
        let mut reader = client(16)
            .open(&original, NetworkAuth::S3(credentials))
            .expect("open authenticated S3 URL");

        assert_eq!(original, reader.source_name());
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("read S3 response");
        assert_eq!(b"{}", body.as_slice());

        let requests = server.requests();
        assert_eq!(1, requests.len());
        assert!(requests[0].starts_with("GET /bucket/input.json?X-Amz-Algorithm="));
        assert!(requests[0].contains("X-Amz-Credential=access%2F"));
        assert!(requests[0].contains("X-Amz-Security-Token=session%2B%2F%3D"));
        assert!(requests[0].contains("X-Amz-Signature="));
        assert!(!requests[0].contains("discard=this-query"));
    }

    #[test]
    fn s3_virtual_host_region_and_error_surface_are_explicit() {
        let credentials = S3CredentialsRef::new("access", "secret", None);
        let signing_time = UNIX_EPOCH + Duration::from_secs(1_788_111_254);
        let signed = presign_s3_get_at(
            "https://bucket.s3.ca-central-1.amazonaws.com/logs/a%20b.json?old=1",
            credentials,
            signing_time,
        )
        .expect("sign virtual-host URL");
        assert!(signed.contains("access%2F20260830%2Fca-central-1%2Fs3%2Faws4_request"));
        assert!(signed.starts_with(
            "https://bucket.s3.ca-central-1.amazonaws.com/logs/a%20b.json?X-Amz-Algorithm="
        ));
        assert!(!signed.contains("old=1"));

        let dotted_bucket = presign_s3_get_at(
            "https://foo.s3.bar.s3.ca-central-1.amazonaws.com/key",
            credentials,
            signing_time,
        )
        .expect("greedy virtual-host bucket matches C++");
        assert!(dotted_bucket.contains("access%2F20260830%2Fca-central-1%2Fs3%2Faws4_request"));

        let explicit_default_port = presign_s3_get_at(
            "http://example.com:80/bucket/key",
            credentials,
            signing_time,
        )
        .expect("sign explicit default port");
        assert!(explicit_default_port.starts_with("http://example.com:80/bucket/key?"));

        assert_eq!(
            S3PresignError::UnsupportedScheme,
            presign_s3_get_at("s3://bucket/key", credentials, signing_time)
                .expect_err("s3 scheme is unsupported")
        );
        assert_eq!(
            S3PresignError::InvalidS3Url,
            presign_s3_get_at("https://example.com/no-key", credentials, signing_time)
                .expect_err("path-style URL requires bucket and key")
        );
        assert_eq!(
            S3PresignError::TimeBeforeUnixEpoch,
            presign_s3_get_at(
                "https://example.com/bucket/key",
                credentials,
                UNIX_EPOCH - Duration::from_secs(1),
            )
            .expect_err("pre-epoch time must fail")
        );
    }

    #[test]
    fn credential_debug_and_errors_do_not_expose_secrets_or_signed_queries() {
        let credentials =
            S3CredentialsRef::new("access-secret", "very-secret", Some("session-secret"));
        let debug = format!("{credentials:?} {:?}", NetworkAuth::S3(credentials));
        for secret in ["access-secret", "very-secret", "session-secret"] {
            assert!(!debug.contains(secret));
        }

        let error = HttpOpenError::Transport(HttpTransportErrorKind::Connect);
        assert!(!format!("{error:?} {error}").contains("X-Amz"));

        let server = MockServer::start(|_| content_length_response("200 OK", b"", ""));
        let reader = client(1)
            .open(&server.url("/input?caller-secret=value"), NetworkAuth::None)
            .expect("open source with sensitive query");
        assert!(!format!("{reader:?}").contains("caller-secret"));
    }

    #[test]
    fn forward_seek_discards_without_buffering_and_reports_unsupported_moves() {
        let mut reader = ForwardSeekReader::new(Cursor::new(b"abcdefgh"));
        assert_eq!(3, reader.seek(SeekFrom::Start(3)).expect("seek forward"));
        let mut output = [0_u8; 2];
        reader.read_exact(&mut output).expect("read after skip");
        assert_eq!(*b"de", output);
        assert_eq!(5, reader.position());
        assert_eq!(7, reader.seek(SeekFrom::Current(2)).expect("skip current"));

        let backward = reader
            .seek(SeekFrom::Start(1))
            .expect_err("backward seek must fail");
        assert_eq!(io::ErrorKind::Unsupported, backward.kind());
        let backward = backward
            .get_ref()
            .and_then(|error| error.downcast_ref::<ForwardSeekError>())
            .copied();
        assert_eq!(
            Some(ForwardSeekError::Backward {
                current: 7,
                requested: 1,
            }),
            backward
        );

        let end_relative = reader
            .seek(SeekFrom::End(0))
            .expect_err("end-relative seek must fail");
        assert_eq!(io::ErrorKind::Unsupported, end_relative.kind());

        let debug = format!("{reader:?}");
        assert!(debug.contains("scratch_bytes: 65536"));
        assert!(debug.len() < 512);
    }

    #[test]
    fn forward_seek_cannot_bypass_the_underlying_response_limit() {
        let server = MockServer::start(|_| chunked_response(&[b"12345"]));
        let http = client(4)
            .open(&server.url("/over"), NetworkAuth::None)
            .expect("open over-limit response");
        let mut reader = ForwardSeekReader::new(http);

        let first = reader
            .seek(SeekFrom::Start(5))
            .expect_err("seek must pass through the response limit");
        assert_eq!(4, reader.position());
        let typed = first
            .get_ref()
            .and_then(|error| error.downcast_ref::<HttpReadError>())
            .copied();
        assert_eq!(
            Some(HttpReadError::LimitExceeded(HttpLimitViolation::new(5, 4))),
            typed
        );

        assert!(reader.seek(SeekFrom::Start(5)).is_err());
        assert_eq!(4, reader.position());
    }

    #[test]
    fn streams_real_cpp_sfa_through_content_length_and_chunked_http() {
        for chunked in [false, true] {
            let server = MockServer::start(move |_| {
                if chunked {
                    let split = CPP_SFA.len() / 2;
                    chunked_response(&[&CPP_SFA[..split], &CPP_SFA[split..]])
                } else {
                    content_length_response("200 OK", CPP_SFA, "")
                }
            });
            let http = client(1024)
                .open(&server.url("/archive.sfa?source=yes"), NetworkAuth::None)
                .expect("open remote SFA");
            let content_length = http.content_length();
            let source = ForwardSeekReader::new(http);
            let mut archive = SingleFileArchiveReader::open_streaming(source, content_length)
                .expect("open streaming SFA");
            let mut output = Vec::new();
            let stats = extract_jsonl(&mut archive, &mut output, ExtractionOptions::default())
                .expect("extract remote SFA");

            assert_eq!(1, stats.records());
            assert_eq!(
                concat!(
                    "{\"ts\":1700000000123,\"level\":\"INFO\",",
                    "\"message\":\"oracle fixture\",\"value\":42,\"active\":true}\n",
                )
                .as_bytes(),
                output.as_slice()
            );
            assert_eq!(1, server.requests().len());
        }
    }
}
