//! ```
//! # tokio_test::block_on(async {
//! use bytes::Bytes;
//! use futures::{future, stream, StreamExt};
//! use multipart::{Boundary, Multipart};
//!
//! let data = "Content-Type: text/plain\r
//! \r
//! Hello World!
//! --xoxo";
//! let mut multipart = Multipart::new(Boundary::new("xoxo"), stream::once(future::ready(Result::<Bytes, ()>::Ok(data.into()))));
//!
//! while let Some(res) = multipart.next().await {
//!     let mut part = res?;
//!     let mut body = String::new();
//!
//!     while let Some(res) = part.next().await {
//!         body.push_str(std::str::from_utf8(&res?).unwrap());
//!     }
//!
//!     assert!(body.len() > 0);
//! }
//! # Result::<(), multipart::MultipartError<()>>::Ok(())
//! # });
//! ```
#![deny(
    unreachable_pub,
    missing_debug_implementations,
    missing_docs,
    clippy::pedantic
)]

use std::{
    cell::RefCell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures::{ready, Stream, TryStream};
use http::{
    header::{self, HeaderName, InvalidHeaderName, InvalidHeaderValue},
    HeaderMap, HeaderValue,
};
use httparse::parse_headers;
use memchr::memmem;
use mime::Mime;

/// A part of a multipart body.
#[derive(Debug)]
pub struct BodyPart<S> {
    headers: HeaderMap,
    inner: Rc<RefCell<InnerMultipart<S>>>,
    done: bool,
}

impl<S> Drop for BodyPart<S> {
    fn drop(&mut self) {
        self.inner.borrow_mut().state = State::Skip;
    }
}

#[derive(Debug)]
struct InnerMultipart<S> {
    boundary: Boundary,
    state: State,
    stream: S,
    buf: BytesMut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Skip to the next boundary.
    Skip,
    Headers,
    Body,
    Eof,
}

/// A multipart request.
///
/// **Each body part must be either read to completion or dropped before polling the next
/// body part, since they share the underlying stream.**
#[derive(Debug)]
pub struct Multipart<S> {
    inner: Rc<RefCell<InnerMultipart<S>>>,
}

impl<S> Multipart<S>
where
    S: Stream,
{
    /// Construct a new parser from a given boundary and payload.
    pub fn new(boundary: Boundary, payload: S) -> Self {
        Self {
            inner: Rc::new(RefCell::new(InnerMultipart {
                boundary,
                stream: payload,
                buf: BytesMut::new(),
                state: State::Skip,
            })),
        }
    }
}

impl<S, E> Stream for Multipart<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<BodyPart<S>, MultipartError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut r = self.inner.borrow_mut();

        match r.state {
            State::Skip | State::Headers => {}
            State::Body => panic!("drop the previous body part before polling next"),
            State::Eof => return Poll::Ready(None),
        }

        if r.buf.is_empty() {
            match ready!(r.poll_extend(cx)) {
                Ok(()) => {}
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }

        if r.state == State::Skip {
            match r.skip_until_boundary(false) {
                Some(false) => r.state = State::Headers,
                Some(true) => {
                    r.state = State::Eof;
                    return Poll::Ready(None);
                }
                None => {
                    // buffer exhausted without meeting a boundary
                    drop(r);
                    return self.poll_next(cx);
                }
            }
        }

        // state is guaranteed to be State::Headers
        loop {
            match r.read_headers() {
                Ok(Some(headers)) => {
                    let part = BodyPart::new(headers, self.inner.clone());
                    r.state = State::Body;
                    return Poll::Ready(Some(Ok(part)));
                }
                Ok(None) => match ready!(r.poll_extend(cx)) {
                    Ok(()) => continue, // read headers again
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }
    }
}

impl<S> BodyPart<S> {
    fn new(headers: HeaderMap, reader: Rc<RefCell<InnerMultipart<S>>>) -> Self {
        Self {
            inner: reader,
            headers,
            done: false,
        }
    }

    /// Get the headers associated with this body part.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get and parse the `Content-Type`.
    #[must_use]
    pub fn content_type(&self) -> Option<Mime> {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
    }
}

impl<S, E> Stream for BodyPart<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, MultipartError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut r = self.inner.borrow_mut();

        if self.done {
            return Poll::Ready(None);
        }

        assert!(!(r.state != State::Body), "wrong state");

        if r.buf.is_empty() {
            match ready!(r.poll_extend(cx)) {
                Ok(()) => {}
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }

        let (bytes, boundary) = r.read_until_boundary();

        match boundary {
            Some(eof) => {
                if eof {
                    r.state = State::Eof;
                } else {
                    r.state = State::Skip;
                }

                drop(r);

                self.get_mut().done = true;

                Poll::Ready(Some(Ok(bytes)))
            }
            None => Poll::Ready(Some(Ok(bytes))),
        }
    }
}

const MAX_HEADERS: usize = 32;

/// Maximum size of the header section for each
/// body part. (16 KiB).
const MAX_HEADER_SECTION_SIZE: usize = 16 << 10;

impl<S> InnerMultipart<S> {
    /// Search for a needle, and return all bytes up to and **including
    /// the needle**, if it's found.
    ///
    /// If no needle is found, no bytes are returned and the buffer
    /// is unaffected.
    fn read_until(&mut self, needle: &[u8]) -> Option<Bytes> {
        memmem::find(&self.buf, needle).map(|idx| self.buf.split_to(idx + needle.len()).freeze())
    }

    fn read_headers<E>(&mut self) -> Result<Option<HeaderMap>, MultipartError<E>> {
        let bytes = if let Some(bytes) = self.read_until(b"\r\n\r\n") {
            bytes
        } else {
            if self.buf.len() > MAX_HEADER_SECTION_SIZE {
                return Err(MultipartError::ParseHeaders(
                    ParseHeadersError::HeaderOverflow,
                ));
            }

            return Ok(None);
        };

        let mut hdrs = [httparse::EMPTY_HEADER; MAX_HEADERS];

        match parse_headers(&bytes, &mut hdrs) {
            Ok(httparse::Status::Complete((_, hdrs))) => {
                let mut headers = HeaderMap::with_capacity(hdrs.len());

                for h in hdrs {
                    let name = HeaderName::try_from(h.name)?;
                    let value = HeaderValue::try_from(h.value)?;

                    headers.append(name, value);
                }

                Ok(Some(headers))
            }
            Ok(httparse::Status::Partial) => Err(MultipartError::ParseHeaders(
                ParseHeadersError::HeaderOverflow,
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Read until a boundary is found, or the buffer is empty. The boundary
    /// itself is kept in the buffer.
    fn read_until_boundary(&mut self) -> (Bytes, Option<bool>) {
        match memmem::find(&self.buf, &[b"--", self.boundary.as_bytes()].concat()) {
            Some(i) => {
                let chunk = self.buf.split_to(i).freeze();

                let boundary_line_len = 2 + self.boundary.len();

                let eof = self.buf.len() >= 2 + boundary_line_len
                    && &self.buf[boundary_line_len..boundary_line_len + 2] == b"--";

                // self.readline(); // remove line containing --boundary

                (chunk, Some(eof))
            }
            None => (self.buf.split().freeze(), None),
        }
    }

    /// Skip until a boundary is found, or to the end of the buffer.
    ///
    /// - `None` -- no boundary found.
    /// - `Some(false)` -- boundary found, but it's not EOF.
    /// - `Some(true)` -- final boundary found (EOF).
    fn skip_until_boundary(&mut self, keep_boundary: bool) -> Option<bool> {
        let (_, boundary) = self.read_until_boundary();

        if boundary.is_some() && !keep_boundary {
            // the next line will be the boundary
            self.readline();
        }

        boundary
    }

    fn readline(&mut self) -> Option<Bytes> {
        self.read_until(b"\n")
    }

    /// Extend the buffer, or return an error if the stream is finished.
    fn poll_extend<E>(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), MultipartError<E>>>
    where
        S: TryStream<Ok = Bytes, Error = E> + Unpin,
    {
        match ready!(Pin::new(&mut self.stream).try_poll_next(cx)) {
            Some(Ok(chunk)) => {
                self.buf.extend_from_slice(&chunk);
                Poll::Ready(Ok(()))
            }
            Some(Err(e)) => Poll::Ready(Err(MultipartError::Upstream(e))),
            None => Poll::Ready(Err(MultipartError::UnexpectedEof)),
        }
    }
}

/// Boundary extraction error.
#[derive(Debug, thiserror::Error)]
pub enum InvalidBoundary {
    /// `Content-Type` header missing.
    #[error("missing content-type")]
    NoContentType,

    /// `Content-Type` could not be parsed.
    #[error("failed to parse the content-type header")]
    ParseContentType,

    /// No boundary specified.
    #[error("no boundary specified")]
    NoBoundary,
}

impl From<mime::FromStrError> for InvalidBoundary {
    fn from(_: mime::FromStrError) -> Self {
        Self::ParseContentType
    }
}

/// A multipart boundary.
#[derive(Debug)]
pub struct Boundary(String);

impl AsRef<str> for Boundary {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Boundary {
    /// Construct a new [`Boundary`].
    pub fn new(data: impl Into<String>) -> Self {
        Self(data.into())
    }

    /// Convert a header into a [`Boundary`].
    ///
    /// # Errors
    ///
    /// - invalid mime type
    /// - mime cannot be parsed to boundary
    pub fn try_from_header(h: &str) -> Result<Self, InvalidBoundary> {
        Self::try_from(&h.parse::<Mime>()?)
    }

    /// Get the length of the boundary in bytes.
    #[must_use]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Get the boundary as bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl TryFrom<&Mime> for Boundary {
    type Error = InvalidBoundary;

    fn try_from(value: &Mime) -> Result<Self, Self::Error> {
        value
            .get_param(mime::BOUNDARY)
            .ok_or(InvalidBoundary::NoBoundary)
            .map(|n| Self::new(n.to_string()))
    }
}

impl TryFrom<&HeaderMap> for Boundary {
    type Error = InvalidBoundary;

    fn try_from(value: &HeaderMap) -> Result<Self, Self::Error> {
        let ct = value
            .get(header::CONTENT_TYPE)
            .ok_or(InvalidBoundary::NoContentType)?
            .to_str()
            .map_err(|_| InvalidBoundary::ParseContentType)?;

        Boundary::try_from_header(ct)
    }
}

#[cfg(feature = "actix")]
impl TryFrom<&actix_http::header::HeaderMap> for Boundary {
    type Error = InvalidBoundary;

    fn try_from(value: &actix_http::header::HeaderMap) -> Result<Self, Self::Error> {
        let ct = value
            .get(header::CONTENT_TYPE)
            .ok_or(InvalidBoundary::NoContentType)?
            .to_str()
            .map_err(|_| InvalidBoundary::ParseContentType)?;

        Boundary::try_from_header(ct)
    }
}

/// Headers parsing error.
#[derive(Debug, thiserror::Error)]
pub enum ParseHeadersError {
    /// Failed to parse the HTTP header section.
    #[error("{0}")]
    Http(#[from] httparse::Error),

    /// Invalid header name.
    #[error("{0}")]
    InvalidName(#[from] InvalidHeaderName),

    /// Invalid header value.
    #[error("{0}")]
    InvalidValue(#[from] InvalidHeaderValue),

    /// Too many headers specified.
    #[error("too many headers")]
    HeaderOverflow,
}

/// An error.
#[derive(Debug, thiserror::Error)]
pub enum MultipartError<E> {
    /// Upstream error.
    #[error("{0}")]
    Upstream(E),

    /// Unexpected EOF
    #[error("unexpected eof")]
    UnexpectedEof,

    /// Failed to parse headers.
    #[error("parse headers failed: {0}")]
    ParseHeaders(ParseHeadersError),

    /// Boundary extraction error.
    #[error("cannot get boundary: {0}")]
    Boundary(#[from] InvalidBoundary),
}

impl<E> From<httparse::Error> for MultipartError<E> {
    fn from(e: httparse::Error) -> Self {
        Self::ParseHeaders(ParseHeadersError::Http(e))
    }
}

impl<E> From<InvalidHeaderName> for MultipartError<E> {
    fn from(e: InvalidHeaderName) -> Self {
        Self::ParseHeaders(ParseHeadersError::InvalidName(e))
    }
}

impl<E> From<InvalidHeaderValue> for MultipartError<E> {
    fn from(e: InvalidHeaderValue) -> Self {
        Self::ParseHeaders(ParseHeadersError::InvalidValue(e))
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use futures::{future, stream, StreamExt};

    use crate::{Boundary, Multipart};

    #[tokio::test]
    async fn simple_multipart() {
        const BODY: &[u8] = b"--xoxo\r\nContent-Type: text/plain\r\n\r\nYooo\n--xoxo\r\nContent-Type: text/plain\r\n\r\nhello there\n--xoxo--";

        let mut multipart = Multipart::new(
            Boundary::new("xoxo"),
            stream::once(future::ready(Result::<_, ()>::Ok(BODY.into()))),
        );

        let first = multipart.next().await.unwrap().unwrap();
        assert_eq!(
            first.headers().get("content-type"),
            Some(&"text/plain".parse().unwrap())
        );
        drop(first);

        let mut second = multipart.next().await.unwrap().unwrap();
        let mut data = BytesMut::new();
        while let Some(res) = second.next().await {
            data.extend(res.unwrap());
        }

        assert_eq!(data.freeze(), b"hello there\n".as_slice());
    }

    // The previous part needs to be dropped before the next is read, since they share
    // the underlying stream. This should therefore panic.
    #[tokio::test]
    #[should_panic]
    async fn next_part_without_drop() {
        const BODY: &[u8] = b"--xoxo\r\nContent-Type: text/plain\r\n\r\nYooo\n--xoxo\r\nContent-Type: text/plain\r\n\r\nhello there\n--xoxo--";

        let mut multipart = Multipart::new(
            Boundary::new("xoxo"),
            stream::once(future::ready(Result::<_, ()>::Ok(BODY.into()))),
        );

        let _first = multipart.next().await.unwrap().unwrap();
        let _second = multipart.next().await.unwrap().unwrap(); // panic here
    }
}
