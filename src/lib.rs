#![doc = include_str!("../README.md")]
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
use futures_core::{Stream, TryStream};
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
    boundary: String,
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
#[derive(Debug)]
pub struct Multipart<S> {
    inner: Rc<RefCell<InnerMultipart<S>>>,
}

impl<S> Multipart<S> {
    /// Construct a new parser from a given boundary and payload.
    pub fn from_boundary(boundary: impl Into<String>, payload: S) -> Self {
        Self {
            inner: Rc::new(RefCell::new(InnerMultipart {
                boundary: boundary.into(),
                stream: payload,
                buf: BytesMut::new(),
                state: State::Skip,
            })),
        }
    }

    /// Construct a new parser from some headers and a payload.
    ///
    /// # Errors
    ///
    /// If the boundary cannot be properly extracted from the `Content-Type`
    /// header, this function will return an error.
    pub fn new<E>(headers: &HeaderMap, payload: S) -> Result<Self, MultipartError<E>> {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .ok_or(MultipartError::NoContentType)?
            .to_str()
            .ok()
            .and_then(|s| s.parse::<Mime>().ok())
            .ok_or(MultipartError::ParseContentType)?;

        let boundary = content_type
            .get_param(mime::BOUNDARY)
            .ok_or(MultipartError::Boundary)?
            .as_str();

        Ok(Self::from_boundary(boundary, payload))
    }
}

impl<S, E> Stream for Multipart<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<BodyPart<S>, MultipartError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut r = match self.inner.try_borrow_mut() {
            Ok(r) => match r.state {
                State::Skip | State::Headers => r,
                State::Body => return Poll::Pending,
                State::Eof => return Poll::Ready(None),
            },
            Err(_) => return Poll::Pending, // the reader is in use by a body part
        };

        if r.buf.is_empty() {
            match r.poll_extend(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }

        if r.state == State::Skip {
            match r.skip_until_boundary(false) {
                Some(false) => r.state = State::Headers,
                Some(true) => {
                    r.state = State::Eof;
                    return Poll::Ready(None);
                }
                None => return Poll::Pending,
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
                Ok(None) => match r.poll_extend(cx) {
                    Poll::Ready(Ok(())) => continue, // read headers again
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                    Poll::Pending => return Poll::Pending,
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
}

impl<S, E> Stream for BodyPart<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, MultipartError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut r = self.inner.try_borrow_mut().unwrap();

        if self.done {
            return Poll::Ready(None);
        }

        assert!(!(r.state != State::Body), "wrong state");

        if r.buf.is_empty() {
            match r.poll_extend(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
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
        match Pin::new(&mut self.stream).try_poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.buf.extend_from_slice(&chunk);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(MultipartError::Upstream(e))),
            Poll::Ready(None) => Poll::Ready(Err(MultipartError::UnexpectedEof)),
            Poll::Pending => Poll::Pending,
        }
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
    #[error("{0}")]
    ParseHeaders(ParseHeadersError),

    /// No `Content-Type` specified on the main request.
    #[error("missing content-type header")]
    NoContentType,

    /// The `Content-Type` could not be properly parsed.
    #[error("failed to parse the content-type header")]
    ParseContentType,

    /// No boundary specified.
    #[error("no boundary specified")]
    Boundary,
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
