//! This library provides the [`RequestFile`] type that
//! provides an asynchronous file-like interface to a web resource.
//!
//! # Features
//!
//!   * No unsafe code (`#[forbid(unsafe_code)]`)
//!   * Tested; code coverage: 100%
//!   * Supports either `futures_util::io::{AsyncRead, AsyncSeek}` or `tokio::io::{AsyncRead, AsyncSeek}` (via feature flags)
//!
//! # Examples
//!
//! ```
//! # tokio_test::block_on(async {
//! use reqwest_file::RequestFile;
//! use tokio::io::{AsyncReadExt, AsyncSeekExt};
//!
//! let client = reqwest::Client::new();
//! let request = client.get("http://httpbin.org/base64/aGVsbG8gd29ybGQ=");
//! let mut file: RequestFile = RequestFile::new(request);
//!
//! let mut buffer = [0; 5];
//! assert_eq!(file.read(&mut buffer).await.unwrap(), 5);
//! assert_eq!(&buffer, b"hello");
//!
//! let mut buffer = [0; 5];
//! assert_eq!(file.seek(std::io::SeekFrom::Current(1)).await.unwrap(), 6);
//! assert_eq!(file.read(&mut buffer).await.unwrap(), 5);
//! assert_eq!(&buffer, b"world");
//! # })
//! ```
#![cfg_attr(feature = "nightly", feature(type_alias_impl_trait, io_error_more))]
#![forbid(unsafe_code)]

use std::future::Future;
use std::io::{Error as IoError, ErrorKind, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::inner::{send_request, RequestStream, RequestStreamFuture};
use bytes::Bytes;
use futures_util::{ready, FutureExt, StreamExt};
use pin_project::pin_project;
use reqwest::{RequestBuilder, StatusCode};

#[cfg(feature = "futures_io")]
use futures_util::io::{AsyncRead, AsyncSeek, AsyncSeekExt};
#[cfg(feature = "futures_io")]
use futures_util::stream::IntoAsyncRead;
#[cfg(feature = "futures_io")]
use futures_util::TryStreamExt;

#[cfg(feature = "tokio_io")]
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt, ReadBuf};
#[cfg(feature = "tokio_io")]
use tokio_util::io::StreamReader;

fn to_io_error(e: impl std::error::Error + Send + Sync + 'static) -> IoError {
    IoError::new(ErrorKind::Other, e)
}

#[derive(Debug)]
struct HttpResponseStatusError(StatusCode);

impl std::fmt::Display for HttpResponseStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP status error: {}", self.0)
    }
}

impl std::error::Error for HttpResponseStatusError {}

// Due to changes in TAIT handling, TAITs cannot remain unconstrained if they appear
// in a signature. In the long run, the `#[defines]` attribute will be used to mark
// defining methods. Until then, the current workaround is to move the TAIT declaration
// as well as all methods that actually constrain its hidden type into its own submodule.
// see: https://github.com/rust-lang/rust/pull/113169
#[cfg(feature = "nightly")]
mod inner {
    use super::*;

    // Unfortunately, `reqwest::Response::bytes_stream()` returns
    // an unnamed type. Since we need to store this type,
    // we need to be able to name it, thus requiring the TAIT here.
    pub type RequestStream =
        impl futures_util::Stream<Item = Result<Bytes, IoError>>;
    pub type StreamData = (Option<u64>, RequestStream);
    pub type RequestStreamFuture =
        impl Future<Output = Result<StreamData, IoError>>;

    /// Send a request with a `Range` header,
    /// returning a future for the response stream.
    pub fn send_request(
        request: &RequestBuilder,
        offset: u64,
        size: Option<u64>,
    ) -> RequestStreamFuture {
        let request = prepare_request(request, offset, size);
        request.send().map(move |result| {
            result
                .and_then(|response| response.error_for_status())
                .map_err(to_io_error)
                .and_then(|response| {
                    let size = process_response(&response, offset)?;
                    let stream = response
                        .bytes_stream()
                        .map(|result| result.map_err(to_io_error));

                    Ok((size, stream))
                })
        })
    }
}

#[cfg(not(feature = "nightly"))]
mod inner {
    use super::*;
    use futures_util::future::BoxFuture;
    use futures_util::stream::BoxStream;

    pub type RequestStream = BoxStream<'static, Result<Bytes, IoError>>;
    pub type StreamData = (Option<u64>, RequestStream);
    pub type RequestStreamFuture =
        BoxFuture<'static, Result<StreamData, IoError>>;

    /// Send a request with a `Range` header,
    /// returning a future for the response stream.
    pub fn send_request(
        request: &RequestBuilder,
        offset: u64,
        size: Option<u64>,
    ) -> RequestStreamFuture {
        let request = prepare_request(request, offset, size);
        Box::pin(request.send().map(move |result| {
            result
                .and_then(|response| response.error_for_status())
                .map_err(to_io_error)
                .and_then(|response| {
                    let size = process_response(&response, offset)?;
                    let stream = response
                        .bytes_stream()
                        .map(|result| result.map_err(to_io_error))
                        .boxed();

                    Ok((size, stream))
                })
        }))
    }
}

fn prepare_request(
    request: &RequestBuilder,
    offset: u64,
    size: Option<u64>,
) -> RequestBuilder {
    let range = match size {
        Some(size) if size > 0 => {
            let end = size - 1;
            format!("bytes={offset}-{end}")
        }
        _ => {
            format!("bytes={offset}-")
        }
    };

    let request = request
        .try_clone()
        .expect("request contains streaming body");
    request.header(reqwest::header::RANGE, range)
}

fn process_response(
    response: &reqwest::Response,
    offset: u64,
) -> Result<Option<u64>, IoError> {
    if response.status() == StatusCode::OK {
        if offset != 0 {
            #[cfg(feature = "nightly")]
            return Err(ErrorKind::NotSeekable.into());
            #[cfg(not(feature = "nightly"))]
            return Err(ErrorKind::Unsupported.into());
        }
    } else if response.status() != StatusCode::PARTIAL_CONTENT {
        let error = HttpResponseStatusError(response.status());
        return Err(to_io_error(error));
    }
    Ok(determine_size(offset, response))
}

fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        // convert to str (eg. "123")
        .to_str()
        .ok()?
        // convert to an integer
        .parse::<u64>()
        .ok()
}

fn parse_range_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_RANGE)?
        // convert to str (eg. "bytes 3-5/7")
        .to_str()
        .ok()?
        // take the part after '/'
        .split_once('/')?
        .1
        // convert to an integer
        .parse::<u64>()
        .ok()
}

fn determine_size(offset: u64, response: &reqwest::Response) -> Option<u64> {
    if response.status() == StatusCode::OK {
        parse_content_length(response.headers())
    } else if response.status() == StatusCode::PARTIAL_CONTENT {
        parse_range_length(response.headers()).or_else(|| {
            parse_content_length(response.headers()).map(|size| offset + size)
        })
    } else {
        unreachable!()
    }
}

#[cfg(feature = "tokio_io")]
fn fastforward<R: AsyncRead + Unpin, const BUFFER: usize>(
    reader: Pin<&mut R>,
    delta: u64,
    context: &mut Context<'_>,
) -> (u64, u64, Poll<Result<(), IoError>>) {
    fastforward_impl::<R, _, BUFFER>(
        reader,
        delta,
        context,
        |reader, context, buffer| {
            let mut array = [std::mem::MaybeUninit::uninit(); BUFFER];
            let buffer_size = buffer.len();
            let mut read_buf = ReadBuf::uninit(&mut array[0..buffer_size]);
            match reader.poll_read(context, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        },
    )
}

#[cfg(feature = "futures_io")]
fn fastforward<R: AsyncRead + Unpin, const BUFFER: usize>(
    reader: Pin<&mut R>,
    delta: u64,
    context: &mut Context<'_>,
) -> (u64, u64, Poll<Result<(), IoError>>) {
    fastforward_impl::<R, _, BUFFER>(
        reader,
        delta,
        context,
        |reader, context, buffer| match reader.poll_read(context, buffer) {
            Poll::Ready(Ok(read)) => Poll::Ready(Ok(read)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        },
    )
}

/// Try to read `delta` bytes from a stream,
/// returning how many bytes were read and remain to be read.
///
/// Return `Ok` if the fastforward is complete,
/// or if EOF is reached (at the first empty read).
fn fastforward_impl<R, F, const BUFFER: usize>(
    mut reader: Pin<&mut R>,
    delta: u64,
    context: &mut Context<'_>,
    poll_read_fn: F,
) -> (u64, u64, Poll<Result<(), IoError>>)
where
    R: Unpin,
    F: FnMut(
        Pin<&mut R>,
        &mut Context<'_>,
        &mut [u8],
    ) -> Poll<Result<usize, IoError>>,
{
    let mut buffer = vec![0u8; BUFFER];
    let mut remaining = delta;
    let mut poll_read_fn = poll_read_fn;

    let poll = loop {
        assert!(remaining > 0);
        let buffer_size = (remaining as usize).min(BUFFER);
        let buffer = &mut buffer[..buffer_size];
        match poll_read_fn(reader.as_mut(), context, buffer) {
            Poll::Ready(Ok(read)) => {
                if read == 0 {
                    // reached EOF
                    break Poll::Ready(Ok(()));
                } else {
                    remaining = remaining.checked_sub(read as u64).unwrap();
                    if remaining == 0 {
                        break Poll::Ready(Ok(()));
                    } else {
                        continue;
                    }
                }
            }
            Poll::Ready(Err(e)) => break Poll::Ready(Err(e)),
            Poll::Pending => break Poll::Pending,
        }
    };
    let read = delta.checked_sub(remaining).unwrap();
    (read, remaining, poll)
}

/// The maximum fast-forward read length.
const DEFAULT_FF_WINDOW: u64 = 128 * 1024;

/// The size of the fast-forward read buffer.
const DEFAULT_FF_BUFFER: usize = 4096;

/// State of the request file.
enum State<P, R> {
    /// The initial state, before a request is sent.
    Initial,
    /// The request is being sent.
    Pending(Pin<Box<P>>),
    /// The request is finished, the response stream is ready.
    Ready(Pin<Box<R>>),
    /// The response body is fast-forward seeking.
    Seeking(Pin<Box<R>>, u64),
    /// This state should never persist after a function call,
    /// it exists solely to please the borrow-checker.
    Transient,
}

/// Type that provides an asynchronous file-like interface to a web resource.
///
/// This type implements [`AsyncRead`] and [`AsyncSeek`],
/// so it can be used like an asynchronous file.
///
/// Seeking is implemented as sending out a new request
/// with a `Range` header.
/// All http requests made by this type include this header.
///
/// If the webserver does not support range requests,
/// seeking to anything other than the start of the file
/// will either return a [`NotSeekable`] or [`Unsupported`] error
/// (excluding [fast-forwards](#fast-forward)).
/// Note that the `Accept-Ranges` response header is not
/// used to check range request support before sending one.
///
/// If the webserver does not provide the `Content-Length` header,
/// and no size was given using [`RequestFile::with_size()`],
/// then seeking relative to the file end
/// will return an [`Unsupported`] error.
///
/// If the http request fails during a `read` or `seek`
/// operation, it returns an [`Other`] error that wraps the
/// original http error.
/// If the webserver returns anything other than status code
/// `206 Partial Content`, or `200 Ok` for responses
/// starting at the first byte, that is also considered a failure.
///
/// For transient errors that require a new HTTP request,
/// the [`reset()`](RequestFile::reset) method can be used.
///
/// # Assumptions
///
/// This type assumes that the http resource
/// is of constant size, and thus that the separate requests
/// performed while seeking are all consistent.
///
/// # Reading
///
/// Reads are implemented by wrapping the response body
/// stream in either [`StreamReader`] or [`IntoAsyncRead`].
///
/// # Seeking
///
/// Seeking a position before the first byte will
/// return an [`InvalidInput`] error.
///
/// This type performs no special handling of seeking
/// beyond the end of the file (EOF), so what happens in this case
/// depends on the webserver.
///
/// # Optimizations
///
/// ## Fast-Forward
///
/// As an optimization, seeking forward by a small amount
/// (by default up to 128KiB) will not perform
/// a new request, but rather fast-forward through
/// the response body.
///
/// This type of seek is always allowed,
/// even if the webserver does not support `Range` requests.
///
/// ###### Customization
///
/// The settings for fast-forwards can be changed through
/// two constant parameters.
///
///   * `FF_WINDOW` limits how much can be fast-forwarded;
///     only seeking up to this number of bytes forwards
///     will read through the request (discarding the data)
///     to avoid sending out a new request.
///
///   * `FF_BUFFER` defines the internal buffer size
///     used to read into during a fast-foward.
///
/// [`NotSeekable`]: std::io::ErrorKind::NotSeekable
/// [`Unsupported`]: std::io::ErrorKind::Unsupported
/// [`InvalidInput`]: std::io::ErrorKind::InvalidInput
/// [`Other`]: std::io::ErrorKind::Other
#[pin_project(project = RequestFileProjection)]
pub struct RequestFile<
    const FF_WINDOW: u64 = DEFAULT_FF_WINDOW,
    const FF_BUFFER: usize = DEFAULT_FF_BUFFER,
> {
    /// The request template.
    request: RequestBuilder,
    /// The state of the HTTP request.
    #[cfg(feature = "tokio_io")]
    state: State<RequestStreamFuture, StreamReader<RequestStream, Bytes>>,
    #[cfg(feature = "futures_io")]
    state: State<RequestStreamFuture, IntoAsyncRead<RequestStream>>,
    /// Track the size of the response body.
    size_: Option<u64>,
    /// Track the current position in the response body.
    position: u64,
    /// Track the current seek operation
    #[cfg(feature = "futures_io")]
    seek_position: Option<SeekFrom>,
}

impl<const FF_WINDOW: u64, const FF_BUFFER: usize>
    RequestFile<FF_WINDOW, FF_BUFFER>
{
    /// Create a new file-like object for a web resource.
    ///
    /// # Panics
    ///
    /// This function panics if the request:
    ///  * already includes a `Range` header
    ///  * contains a streaming body
    ///  * building the request fails.
    pub fn new(request: RequestBuilder) -> Self {
        Self::with_size(request, None)
    }

    /// Create a new file-like object for a web resource.
    ///
    /// This function allows you to set the response body size
    /// if you happen to know it before performing the request.
    /// This value must be known---either through this method
    /// or via the `Content-Length` header on the response---to
    /// seek relative to the file end.
    ///
    /// If the response specifies a different size using the `Content-Length`
    /// header, the value given here is overwritten.
    ///
    /// # Panics
    ///
    /// This function panics if the request:
    ///  * already includes a `Range` header
    ///  * contains a streaming body
    ///  * building the request fails.
    pub fn with_size(
        request: RequestBuilder,
        size: impl Into<Option<u64>>,
    ) -> Self {
        request
            // Ensure it can be cloned.
            .try_clone()
            .expect("request contains streaming body")
            // Ensure is valid.
            .build()
            .expect("invalid request")
            // Ensure it does not already contain a range header.
            .headers()
            .contains_key(reqwest::header::RANGE)
            .then(|| panic!("request already has range header set"));
        Self {
            request,
            state: State::Initial,
            size_: size.into(),
            position: 0,
            #[cfg(feature = "futures_io")]
            seek_position: None,
        }
    }

    /// Check the size of this file, which may be unknown.
    pub fn size(&self) -> Option<u64> {
        self.size_
    }

    /// Force a new HTTP request to begin,
    /// without changing the current position.
    ///
    /// This method can be used if the request is broken
    /// in a way that can be fixed by restarting it,
    /// for example due to a transient network issue.
    pub fn reset(&mut self) {
        self.state = State::Initial;
    }

    /// Perform the HTTP request so the response is ready to be read.
    pub async fn prepare(&mut self) -> Result<(), IoError> {
        self.seek(SeekFrom::Current(0)).await.map(|_| ())
    }
}

impl<const FF_WINDOW: u64, const FF_BUFFER: usize>
    RequestFileProjection<'_, FF_WINDOW, FF_BUFFER>
{
    /// Compute the absolute seek position.
    fn resolve_seek_position(
        &self,
        position: SeekFrom,
    ) -> Result<u64, IoError> {
        match position {
            SeekFrom::Start(position) => Ok(position),
            SeekFrom::End(delta) => {
                if let Some(size) = &self.size_ {
                    if let Some(position) = size.checked_add_signed(delta) {
                        Ok(position)
                    } else if delta > 0 {
                        // seek overflow; wrap to maximum
                        Ok(u64::MAX)
                    } else {
                        // seek to negative position
                        Err(ErrorKind::InvalidInput.into())
                    }
                } else {
                    // size not known
                    Err(ErrorKind::Unsupported.into())
                }
            }
            SeekFrom::Current(delta) => {
                if let Some(position) = self.position.checked_add_signed(delta)
                {
                    Ok(position)
                } else if delta > 0 {
                    // seek overflow; wrap to maximum
                    Ok(u64::MAX)
                } else {
                    // seek to negative position
                    Err(ErrorKind::InvalidInput.into())
                }
            }
        }
    }

    /// Drive the state from `State::Initial` to `State::Pending`.
    fn drive_initial(&mut self) {
        if let State::Initial = self.state {
            let future = Box::pin(send_request(
                self.request,
                *self.position,
                *self.size_,
            ));
            *self.state = State::Pending(future);
        }
    }

    /// Drive the state from `State::Pending` to `State::Ready`.
    fn poll_drive_pending(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), IoError>> {
        if let State::Pending(future) = self.state {
            future.as_mut().poll(context).map_ok(move |(size, stream)| {
                *self.size_ = self.size_.or(size);
                #[cfg(feature = "tokio_io")]
                {
                    *self.state =
                        State::Ready(Box::pin(StreamReader::new(stream)));
                }
                #[cfg(feature = "futures_io")]
                {
                    *self.state =
                        State::Ready(Box::pin(stream.into_async_read()));
                }
            })
        } else {
            Poll::Ready(Ok(()))
        }
    }

    /// Drive the state from `State::Seeking` to `State::Ready`.
    fn poll_drive_seeking(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), IoError>> {
        match std::mem::replace(self.state, State::Transient) {
            State::Seeking(mut reader, delta) => {
                let (read, remaining, poll) = fastforward::<_, { FF_BUFFER }>(
                    reader.as_mut(),
                    delta,
                    context,
                );
                *self.position = self.position.saturating_add(read);
                match poll {
                    Poll::Ready(Ok(())) => {
                        // If EOF is reached, there may be a remaining delta.
                        *self.position = self.position.wrapping_add(remaining);
                        *self.state = State::Ready(reader);
                        Poll::Ready(Ok(()))
                    }
                    other => {
                        *self.state = State::Seeking(reader, remaining);
                        other
                    }
                }
            }
            state => {
                *self.state = state;
                Poll::Ready(Ok(()))
            }
        }
    }

    /// Drive the state to `State::Ready`.
    fn poll_drive<'a>(
        &'a mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), IoError>> {
        self.drive_initial();
        assert!(!matches!(self.state, State::Initial));

        ready!(self.poll_drive_pending(context))?;
        assert!(!matches!(self.state, State::Initial));
        assert!(!matches!(self.state, State::Pending(_)));

        ready!(self.poll_drive_seeking(context))?;
        assert!(!matches!(self.state, State::Initial));
        assert!(!matches!(self.state, State::Pending(_)));
        assert!(!matches!(self.state, State::Seeking(_, _)));

        assert!(matches!(self.state, State::Ready(_)));
        Poll::Ready(Ok(()))
    }

    fn prepare_seek(&mut self, final_position: u64) {
        let initial_position = *self.position;
        if initial_position != final_position {
            let delta_forward = final_position.saturating_sub(initial_position);
            if 0 < delta_forward && delta_forward <= FF_WINDOW {
                // seeking forwards by a small leap
                if let State::Ready(reader) =
                    std::mem::replace(self.state, State::Transient)
                {
                    *self.state = State::Seeking(reader, delta_forward);
                } else {
                    *self.position = final_position;
                    *self.state = State::Initial;
                }
            } else {
                // seeking backwards or a large leap forwards
                *self.position = final_position;
                *self.state = State::Initial;
            }
        }
    }
}

#[cfg(feature = "tokio_io")]
impl<const FF_WINDOW: u64, const FF_BUFFER: usize> AsyncRead
    for RequestFile<FF_WINDOW, FF_BUFFER>
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), IoError>> {
        let mut this = self.project();
        let reader = match this.poll_drive(context) {
            Poll::Ready(Ok(())) => match this.state {
                State::Ready(reader) => reader.as_mut(),
                _ => unreachable!(),
            },
            other => return other.map_ok(|_| ()),
        };
        let initial_size = buffer.filled().len();
        reader.poll_read(context, buffer).map_ok(|_| {
            let delta =
                buffer.filled().len().checked_sub(initial_size).unwrap();
            *this.position = this.position.saturating_add(delta as u64);
        })
    }
}

#[cfg(feature = "tokio_io")]
impl<const FF_WINDOW: u64, const FF_BUFFER: usize> AsyncSeek
    for RequestFile<FF_WINDOW, FF_BUFFER>
{
    fn start_seek(
        self: Pin<&mut Self>,
        position: SeekFrom,
    ) -> Result<(), IoError> {
        let mut this = self.project();
        let absolute_position = this.resolve_seek_position(position)?;
        this.prepare_seek(absolute_position);
        Ok(())
    }

    fn poll_complete(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<u64, IoError>> {
        let mut this = self.project();
        this.poll_drive(context).map_ok(|_| *this.position)
    }
}

#[cfg(feature = "futures_io")]
impl<const FF_WINDOW: u64, const FF_BUFFER: usize> AsyncRead
    for RequestFile<FF_WINDOW, FF_BUFFER>
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut this = self.project();
        let reader = match this.poll_drive(cx) {
            Poll::Ready(Ok(())) => match this.state {
                State::Ready(reader) => reader.as_mut(),
                _ => unreachable!(),
            },
            Poll::Pending => {
                return Poll::Pending;
            }
            Poll::Ready(Err(err)) => {
                return Poll::Ready(Err(err));
            }
        };
        reader.poll_read(cx, buf).map_ok(|read| {
            *this.position = this.position.saturating_add(read as u64);
            read
        })
    }
}

#[cfg(feature = "futures_io")]
impl<const FF_WINDOW: u64, const FF_BUFFER: usize> AsyncSeek
    for RequestFile<FF_WINDOW, FF_BUFFER>
{
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<std::io::Result<u64>> {
        let mut this = self.project();
        if this.seek_position != &Some(pos) {
            // Ensure previous seeks have finished before starting a new one
            ready!(this.poll_drive(cx))?;
            let absolute_position = match this.resolve_seek_position(pos) {
                Ok(pos) => pos,
                Err(e) => return Poll::Ready(Err(e)),
            };
            *this.seek_position = Some(pos);
            this.prepare_seek(absolute_position);
        }
        let result = this.poll_drive(cx).map_ok(|_| *this.position);
        if let Poll::Ready(_) = &result {
            *this.seek_position = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::RequestFile;
    use std::io::SeekFrom;
    #[cfg(feature = "tokio_io")]
    use tokio::io::{AsyncReadExt, AsyncSeek, AsyncSeekExt};

    #[cfg(feature = "futures_io")]
    use futures_util::io::{AsyncReadExt, AsyncSeekExt};

    #[derive(Debug, serde::Deserialize)]
    pub struct QueryParams {
        data: String,
        content_length: Option<bool>,
        content_range: Option<bool>,
        status: Option<u16>,
    }

    async fn web_page_main(
        range_header: axum::extract::TypedHeader<axum::headers::Range>,
        axum::extract::Query(query): axum::extract::Query<QueryParams>,
    ) -> (
        axum::http::StatusCode,
        axum::http::header::HeaderMap,
        impl axum::response::IntoResponse,
    ) {
        use axum::http::{
            self,
            header::{HeaderMap, HeaderValue},
            StatusCode,
        };

        let mut headers = HeaderMap::new();

        let range = range_header.iter().next().expect("empty range header");
        let data_len = query.data.len();
        let offset = if let std::ops::Bound::Included(offset) = range.0 {
            offset
        } else {
            unreachable!()
        };

        let response =
            query.data.chars().skip(offset as usize).collect::<String>();
        if query.content_range.unwrap_or(false) {
            let content_range = format!("bytes {offset}-{data_len}/{data_len}");
            headers.insert(
                http::header::CONTENT_RANGE,
                HeaderValue::from_str(&content_range).unwrap(),
            );
        }
        if query.content_length.unwrap_or(false) {
            let content_length = response.len().to_string();
            headers.insert(
                http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&content_length).unwrap(),
            );
        }
        let default_status = StatusCode::PARTIAL_CONTENT.as_u16();
        let status = query.status.unwrap_or(default_status);
        let status = StatusCode::from_u16(status).expect("invalid status code");
        // Stream the response so axum won't add a content-length header.
        let response = axum::body::StreamBody::new(futures_util::stream::iter(
            std::iter::once(std::io::Result::Ok(response)),
        ));
        (status, headers, response)
    }

    async fn web_page_no_range_support() -> &'static str {
        "xyz"
    }

    fn start_server() -> String {
        let app = axum::Router::new()
            .route("/", axum::routing::get(web_page_main))
            .route(
                "/no-range-support",
                axum::routing::get(web_page_no_range_support),
            );
        let server = axum::Server::bind(&"0.0.0.0:0".parse().unwrap())
            .serve(app.into_make_service());
        let address = server.local_addr();
        tokio::spawn(server);
        format!("http://{address}")
    }

    async fn read(file: &mut RequestFile) -> Result<String, std::io::Error> {
        let mut response_data = String::new();
        file.read_to_string(&mut response_data)
            .await
            .map(|_| response_data)
    }

    macro_rules! test {
        (
            $name:ident
            $( $token:tt )*
        ) => {
            // Make versions of the test for various const param combos.
            test!(
                @concrete
                $name
                $( $token )*
            );
            test!(
                @concrete
                @ff_window 2
                @ff_buffer 1
                $name
                $( $token )*
            );
        };
        (
            @concrete
            $( @ff_window $ff_window:literal )?
            $( @ff_buffer $ff_buffer:literal )?
            $name:ident
            [
                $( SeekFrom::$seek_from:ident($offset:literal)
                => ( $($tell:tt)* ) );* $(;)?
            ]
            $( Content-Length = $content_length:literal )?
            $( Content-Range = $content_range:literal )?
            $data:literal => $result:tt
        ) => { paste::paste! {
            #[tokio::test]
            async fn [<
                $name
                $( _ff_window_ $ff_window )?
                $( _ff_buffer_ $ff_buffer )?
            >]() {
                let url = start_server();
                let data = $data;
                let client = reqwest::Client::new();

                #[allow(unused_mut)]
                let mut params = String::new();
                $(
                    if $content_length {
                        params.push_str("&content_length=true");
                    }
                )?
                $(
                    if $content_range {
                        params.push_str("&content_range=true");
                    }
                )?

                let request = client.get(format!("{url}/?data={data}{params}"));
                const FF_WINDOW: u64 = super::DEFAULT_FF_WINDOW
                    $( + $ff_window - super::DEFAULT_FF_WINDOW )?;
                const FF_BUFFER: usize = super::DEFAULT_FF_BUFFER
                    $( + $ff_buffer - super::DEFAULT_FF_BUFFER )?;
                let mut file: RequestFile::<FF_WINDOW, FF_BUFFER>
                    = RequestFile::new(request);

                $(
                    let seek_from = SeekFrom::$seek_from($offset);
                    let seek_result = file.seek(seek_from).await;
                    test!(@check_seek seek_result $($tell)*);
                )*

                let mut response_data = String::new();
                let read_result = file.read_to_string(&mut response_data).await;
                test!(@check_read read_result response_data $result);
            }
        }};
        (
            @check_seek $seek:ident $result:literal
        ) => {
            let pos = $seek.expect("seek error");
            assert_eq!(pos, $result,
                "Seek fail: {pos} != {}", $result);
        };
        (
            @check_seek $seek:ident ErrorKind::$kind:ident
        ) => {
            if let Err(error) = $seek {
                use std::io::ErrorKind;
                let kind = error.kind();
                assert_eq!(kind, ErrorKind::$kind,
                    "Seek fail: {kind} != ErrorKind::{}", ErrorKind::$kind);
            } else {
                panic!("expected seek error")
            }
        };
        (
            @check_read $read:ident $data:ident $result:literal
        ) => {
            $read.expect("read error");
            assert_eq!($data, $result,
                "Read fail: {} != {}", $data, $result);
        };
        (
            @check_read $read:ident $data:ident ErrorKind::$kind:ident
        ) => {
            if let Err(error) = $read {
                use std::io::ErrorKind;
                let kind = error.kind();
                assert_eq!(kind, ErrorKind::$kind
                    "Read fail: {kind} != ErrorKind::{}", ErrorKind::$kind);
            } else {
                panic!("expected read error")
            }
        }
    }

    test! {
        test_read
        []
        "abc" => "abc"
    }

    test! {
        test_from_start_first
        [
            SeekFrom::Start(0) => (0)
        ]
        "abc" => "abc"
    }
    test! {
        test_from_start_middle
        [
            SeekFrom::Start(3) => (3)
        ]
        "abcde" => "de"
    }
    test! {
        test_from_start_last
        [
            SeekFrom::Start(4) => (4)
        ]
        "abcd" => ""
    }
    test! {
        test_from_start_beyond
        [
            SeekFrom::Start(9) => (9)
        ]
        "abcd" => ""
    }

    test! {
        test_from_current_before
        [
            SeekFrom::Start(3) => (3);
            SeekFrom::Current(-4) => (ErrorKind::InvalidInput);
        ]
        "abcd" => "d"
    }
    test! {
        test_from_current_first
        [
            SeekFrom::Start(3) => (3);
            SeekFrom::Current(-3) => (0);
        ]
        "abcd" => "abcd"
    }
    test! {
        test_from_current_middle_backward
        [
            SeekFrom::Start(4) => (4);
            SeekFrom::Current(-2) => (2);
        ]
        "abcdef" => "cdef"
    }
    test! {
        test_from_current_middle_forward
        [
            SeekFrom::Start(3) => (3);
            SeekFrom::Current(3) => (6);
        ]
        "abcdefgh" => "gh"
    }
    test! {
        test_from_current_last
        [
            SeekFrom::Start(3) => (3);
            SeekFrom::Current(3) => (6);
        ]
        "abcd" => ""
    }
    test! {
        test_from_current_beyond
        [
            SeekFrom::Start(3) => (3);
            SeekFrom::Current(4) => (7);
        ]
        "abcd" => ""
    }

    test! {
        test_from_end_before
        [
            SeekFrom::End(-6) => (ErrorKind::InvalidInput)
        ]
        Content-Length = true
        "abcd" => "abcd"
    }
    test! {
        test_from_end_first
        [
            SeekFrom::End(-4) => (0)
        ]
        Content-Range = true
        "abcd" => "abcd"
    }
    test! {
        test_from_end_middle
        [
            SeekFrom::End(-2) => (2)
        ]
        Content-Length = true
        "abcd" => "cd"
    }
    test! {
        test_from_end_last
        [
            SeekFrom::End(0) => (4)
        ]
        Content-Range = true
        "abcd" => ""
    }
    test! {
        test_from_end_beyond
        [
            SeekFrom::End(2) => (6)
        ]
        Content-Length = true
        "abcd" => ""
    }

    test! {
        test_from_end_uknown_size
        [
            SeekFrom::End(2) => (ErrorKind::Unsupported)
        ]
        "abc" => "abc"
    }

    #[tokio::test]
    async fn test_seek_at_initial_state() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/?data=abc"));
        let file: RequestFile = RequestFile::new(request);
        let pos = SeekFrom::Start(4);
        tokio::pin!(file);
        #[cfg(feature = "tokio_io")]
        let pos = {
            // NOTE: We cannot use AsyncReadExt.seek() here
            // since that does a `poll_complete()` before `start_seek()`
            // which leaves the file in the `Ready` state.
            use futures_util::future::poll_fn;

            file.as_mut().start_seek(pos).expect("start seek error");
            poll_fn(|context| file.as_mut().poll_complete(context))
                .await
                .expect("complete seek error")
        };

        #[cfg(feature = "futures_io")]
        let pos = { file.seek(pos).await.expect("seek error") };
        assert_eq!(pos, 4);
    }

    /// Test that the file supports reading the stream
    /// when it is already at EOF.
    #[tokio::test]
    async fn test_read_at_end() {
        let url = start_server();
        let client = reqwest::Client::new();
        let data = "abc";
        let request = client.get(format!("{url}/?data={data}"));
        let mut file: RequestFile = RequestFile::new(request);

        let mut response_data = String::new();
        file.read_to_string(&mut response_data)
            .await
            .expect("read error");
        assert_eq!(response_data, data);
        response_data.clear();

        file.read_to_string(&mut response_data)
            .await
            .expect("read error");
        assert_eq!(response_data, "");
    }

    #[tokio::test]
    async fn test_size_empty() {
        let url = start_server();
        let client = reqwest::Client::new();
        let data = "abcd";
        let request = client.get(format!("{url}/?data={data}"));
        let mut file: RequestFile = RequestFile::new(request);

        assert_eq!(file.size(), None);
        file.prepare().await.unwrap();
        assert_eq!(file.size(), None);
    }

    #[tokio::test]
    async fn test_size_from_content_length() {
        let url = start_server();
        let client = reqwest::Client::new();
        let data = "abcd";
        let url = format!("{url}/?data={data}&content_length=true");
        let request = client.get(url);
        let mut file: RequestFile = RequestFile::new(request);

        assert_eq!(file.size(), None);
        file.seek(SeekFrom::Start(2)).await.unwrap();
        assert_eq!(file.size(), Some(data.len() as u64));
    }

    #[tokio::test]
    async fn test_size_from_content_range() {
        let url = start_server();
        let client = reqwest::Client::new();
        let data = "abcd";
        let url = format!("{url}/?data={data}&content_range=true");
        let request = client.get(url);
        let mut file: RequestFile = RequestFile::new(request);

        assert_eq!(file.size(), None);
        file.seek(SeekFrom::Start(2)).await.unwrap();
        assert_eq!(file.size(), Some(data.len() as u64));
    }

    #[tokio::test]
    #[should_panic(expected = "HTTP status client error (404 Not Found)")]
    async fn test_status_404() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/404"));
        let mut file: RequestFile = RequestFile::new(request);

        match file.prepare().await {
            Ok(()) => unreachable!(),
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::Other);
                if let Some(e) = e.into_inner() {
                    panic!("{e}")
                } else {
                    unreachable!()
                }
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "HTTP status error: 204 No Content")]
    async fn test_status_204_no_content() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/?data=abc&status=204"));
        let mut file: RequestFile = RequestFile::new(request);

        match file.prepare().await {
            Ok(()) => unreachable!(),
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::Other);
                if let Some(e) = e.into_inner() {
                    panic!("{e}")
                } else {
                    unreachable!()
                }
            }
        }
    }

    #[tokio::test]
    async fn test_read_no_range_support() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/no-range-support"));
        let mut file: RequestFile = RequestFile::new(request);

        file.seek(SeekFrom::Start(0)).await.expect("seek error");
        let data = read(&mut file).await.expect("read error");
        assert_eq!(data, "xyz");
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "nightly",
        should_panic(expected = "seek error: Kind(NotSeekable)")
    )]
    #[cfg_attr(
        not(feature = "nightly"),
        should_panic(expected = "seek error: Kind(Unsupported)")
    )]
    async fn test_seek_no_range_support() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/no-range-support"));
        let mut file: RequestFile = RequestFile::new(request);

        // seek beyond the fastforward window
        file.seek(SeekFrom::Start(1_000_000_000))
            .await
            .expect("seek error");
    }

    #[tokio::test]
    async fn test_seek_current_overflow() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/?data=abc"));
        let mut file: RequestFile = RequestFile::new(request);

        let seek_from = SeekFrom::Start(u64::MAX - 10);
        let pos = file.seek(seek_from).await.expect("seek error");
        assert_eq!(pos, u64::MAX - 10);

        let pos = file.seek(SeekFrom::Current(20)).await.expect("seek error");
        assert_eq!(pos, u64::MAX);
    }

    #[tokio::test]
    async fn test_seek_end_overflow() {
        let url = start_server();
        let client = reqwest::Client::new();
        let request = client.get(format!("{url}/?data=abc"));
        let mut file: RequestFile =
            RequestFile::with_size(request, u64::MAX - 10);

        let pos = file.seek(SeekFrom::End(20)).await.expect("seek error");
        assert_eq!(pos, u64::MAX);
    }

    #[tokio::test]
    async fn test_reset() {
        let client = reqwest::Client::new();
        let request = client.get("http://example.com/");
        let mut file: RequestFile = RequestFile::new(request);
        file.reset();
    }
}
