use std::cell::{Ref, RefMut};
use std::str;

use bytes::{Bytes, BytesMut};
use cookie::Cookie;
use encoding::all::UTF_8;
use encoding::label::encoding_from_whatwg_label;
use encoding::types::{DecoderTrap, Encoding};
use encoding::EncodingRef;
use futures::{Async, Future, Poll, Stream};
use http::{header, HeaderMap};
use mime::Mime;
use serde::de::DeserializeOwned;
use serde_urlencoded;

use crate::error::{
    ContentTypeError, CookieParseError, ParseError, PayloadError, ReadlinesError,
    UrlencodedError,
};
use crate::extensions::Extensions;
use crate::header::Header;
use crate::json::JsonBody;
use crate::payload::Payload;

struct Cookies(Vec<Cookie<'static>>);

/// Trait that implements general purpose operations on http messages
pub trait HttpMessage: Sized {
    /// Type of message payload stream
    type Stream;

    /// Read the message headers.
    fn headers(&self) -> &HeaderMap;

    /// Message payload stream
    fn take_payload(&mut self) -> Payload<Self::Stream>;

    /// Request's extensions container
    fn extensions(&self) -> Ref<Extensions>;

    /// Mutable reference to a the request's extensions container
    fn extensions_mut(&self) -> RefMut<Extensions>;

    #[doc(hidden)]
    /// Get a header
    fn get_header<H: Header>(&self) -> Option<H>
    where
        Self: Sized,
    {
        if self.headers().contains_key(H::name()) {
            H::parse(self).ok()
        } else {
            None
        }
    }

    /// Read the request content type. If request does not contain
    /// *Content-Type* header, empty str get returned.
    fn content_type(&self) -> &str {
        if let Some(content_type) = self.headers().get(header::CONTENT_TYPE) {
            if let Ok(content_type) = content_type.to_str() {
                return content_type.split(';').next().unwrap().trim();
            }
        }
        ""
    }

    /// Get content type encoding
    ///
    /// UTF-8 is used by default, If request charset is not set.
    fn encoding(&self) -> Result<EncodingRef, ContentTypeError> {
        if let Some(mime_type) = self.mime_type()? {
            if let Some(charset) = mime_type.get_param("charset") {
                if let Some(enc) = encoding_from_whatwg_label(charset.as_str()) {
                    Ok(enc)
                } else {
                    Err(ContentTypeError::UnknownEncoding)
                }
            } else {
                Ok(UTF_8)
            }
        } else {
            Ok(UTF_8)
        }
    }

    /// Convert the request content type to a known mime type.
    fn mime_type(&self) -> Result<Option<Mime>, ContentTypeError> {
        if let Some(content_type) = self.headers().get(header::CONTENT_TYPE) {
            if let Ok(content_type) = content_type.to_str() {
                return match content_type.parse() {
                    Ok(mt) => Ok(Some(mt)),
                    Err(_) => Err(ContentTypeError::ParseError),
                };
            } else {
                return Err(ContentTypeError::ParseError);
            }
        }
        Ok(None)
    }

    /// Check if request has chunked transfer encoding
    fn chunked(&self) -> Result<bool, ParseError> {
        if let Some(encodings) = self.headers().get(header::TRANSFER_ENCODING) {
            if let Ok(s) = encodings.to_str() {
                Ok(s.to_lowercase().contains("chunked"))
            } else {
                Err(ParseError::Header)
            }
        } else {
            Ok(false)
        }
    }

    /// Load request cookies.
    #[inline]
    fn cookies(&self) -> Result<Ref<Vec<Cookie<'static>>>, CookieParseError> {
        if self.extensions().get::<Cookies>().is_none() {
            let mut cookies = Vec::new();
            for hdr in self.headers().get_all(header::COOKIE) {
                let s =
                    str::from_utf8(hdr.as_bytes()).map_err(CookieParseError::from)?;
                for cookie_str in s.split(';').map(|s| s.trim()) {
                    if !cookie_str.is_empty() {
                        cookies.push(Cookie::parse_encoded(cookie_str)?.into_owned());
                    }
                }
            }
            self.extensions_mut().insert(Cookies(cookies));
        }
        Ok(Ref::map(self.extensions(), |ext| {
            &ext.get::<Cookies>().unwrap().0
        }))
    }

    /// Return request cookie.
    fn cookie(&self, name: &str) -> Option<Cookie<'static>> {
        if let Ok(cookies) = self.cookies() {
            for cookie in cookies.iter() {
                if cookie.name() == name {
                    return Some(cookie.to_owned());
                }
            }
        }
        None
    }

    /// Load http message body.
    ///
    /// By default only 256Kb payload reads to a memory, then
    /// `PayloadError::Overflow` get returned. Use `MessageBody::limit()`
    /// method to change upper limit.
    ///
    /// ## Server example
    ///
    /// ```rust,ignore
    /// # extern crate bytes;
    /// # extern crate actix_web;
    /// # extern crate futures;
    /// # #[macro_use] extern crate serde_derive;
    /// use actix_web::{
    ///     AsyncResponder, FutureResponse, HttpMessage, HttpRequest, Response,
    /// };
    /// use bytes::Bytes;
    /// use futures::future::Future;
    ///
    /// fn index(mut req: HttpRequest) -> FutureResponse<Response> {
    ///     req.body()                     // <- get Body future
    ///        .limit(1024)                // <- change max size of the body to a 1kb
    ///        .from_err()
    ///        .and_then(|bytes: Bytes| {  // <- complete body
    ///            println!("==== BODY ==== {:?}", bytes);
    ///            Ok(Response::Ok().into())
    ///        }).responder()
    /// }
    /// # fn main() {}
    /// ```
    fn body(&mut self) -> MessageBody<Self>
    where
        Self::Stream: Stream<Item = Bytes, Error = PayloadError> + Sized,
    {
        MessageBody::new(self)
    }

    /// Parse `application/x-www-form-urlencoded` encoded request's body.
    /// Return `UrlEncoded` future. Form can be deserialized to any type that
    /// implements `Deserialize` trait from *serde*.
    ///
    /// Returns error:
    ///
    /// * content type is not `application/x-www-form-urlencoded`
    /// * content-length is greater than 256k
    ///
    /// ## Server example
    ///
    /// ```rust,ignore
    /// # extern crate actix_web;
    /// # extern crate futures;
    /// # use futures::Future;
    /// # use std::collections::HashMap;
    /// use actix_web::{FutureResponse, HttpMessage, HttpRequest, Response};
    ///
    /// fn index(mut req: HttpRequest) -> FutureResponse<Response> {
    ///     Box::new(
    ///         req.urlencoded::<HashMap<String, String>>()  // <- get UrlEncoded future
    ///            .from_err()
    ///            .and_then(|params| {  // <- url encoded parameters
    ///                println!("==== BODY ==== {:?}", params);
    ///                Ok(Response::Ok().into())
    ///           }),
    ///     )
    /// }
    /// # fn main() {}
    /// ```
    fn urlencoded<T: DeserializeOwned>(&mut self) -> UrlEncoded<Self, T>
    where
        Self::Stream: Stream<Item = Bytes, Error = PayloadError>,
    {
        UrlEncoded::new(self)
    }

    /// Parse `application/json` encoded body.
    /// Return `JsonBody<T>` future. It resolves to a `T` value.
    ///
    /// Returns error:
    ///
    /// * content type is not `application/json`
    /// * content length is greater than 256k
    ///
    /// ## Server example
    ///
    /// ```rust,ignore
    /// # extern crate actix_web;
    /// # extern crate futures;
    /// # #[macro_use] extern crate serde_derive;
    /// use actix_web::*;
    /// use futures::future::{ok, Future};
    ///
    /// #[derive(Deserialize, Debug)]
    /// struct MyObj {
    ///     name: String,
    /// }
    ///
    /// fn index(mut req: HttpRequest) -> Box<Future<Item = Response, Error = Error>> {
    ///     req.json()                   // <- get JsonBody future
    ///        .from_err()
    ///        .and_then(|val: MyObj| {  // <- deserialized value
    ///            println!("==== BODY ==== {:?}", val);
    ///            Ok(Response::Ok().into())
    ///        }).responder()
    /// }
    /// # fn main() {}
    /// ```
    fn json<T: DeserializeOwned + 'static>(&mut self) -> JsonBody<Self, T>
    where
        Self::Stream: Stream<Item = Bytes, Error = PayloadError> + 'static,
    {
        JsonBody::new(self)
    }

    /// Return stream of lines.
    fn readlines(&mut self) -> Readlines<Self>
    where
        Self::Stream: Stream<Item = Bytes, Error = PayloadError> + 'static,
    {
        Readlines::new(self)
    }
}

impl<'a, T> HttpMessage for &'a mut T
where
    T: HttpMessage,
{
    type Stream = T::Stream;

    fn headers(&self) -> &HeaderMap {
        (**self).headers()
    }

    /// Message payload stream
    fn take_payload(&mut self) -> Payload<Self::Stream> {
        (**self).take_payload()
    }

    /// Request's extensions container
    fn extensions(&self) -> Ref<Extensions> {
        (**self).extensions()
    }

    /// Mutable reference to a the request's extensions container
    fn extensions_mut(&self) -> RefMut<Extensions> {
        (**self).extensions_mut()
    }
}

/// Stream to read request line by line.
pub struct Readlines<T: HttpMessage> {
    stream: Payload<T::Stream>,
    buff: BytesMut,
    limit: usize,
    checked_buff: bool,
    encoding: EncodingRef,
    err: Option<ReadlinesError>,
}

impl<T> Readlines<T>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError>,
{
    /// Create a new stream to read request line by line.
    fn new(req: &mut T) -> Self {
        let encoding = match req.encoding() {
            Ok(enc) => enc,
            Err(err) => return Self::err(err.into()),
        };

        Readlines {
            stream: req.take_payload(),
            buff: BytesMut::with_capacity(262_144),
            limit: 262_144,
            checked_buff: true,
            err: None,
            encoding,
        }
    }

    /// Change max line size. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    fn err(err: ReadlinesError) -> Self {
        Readlines {
            stream: Payload::None,
            buff: BytesMut::new(),
            limit: 262_144,
            checked_buff: true,
            encoding: UTF_8,
            err: Some(err),
        }
    }
}

impl<T> Stream for Readlines<T>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError>,
{
    type Item = String;
    type Error = ReadlinesError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if let Some(err) = self.err.take() {
            return Err(err);
        }

        // check if there is a newline in the buffer
        if !self.checked_buff {
            let mut found: Option<usize> = None;
            for (ind, b) in self.buff.iter().enumerate() {
                if *b == b'\n' {
                    found = Some(ind);
                    break;
                }
            }
            if let Some(ind) = found {
                // check if line is longer than limit
                if ind + 1 > self.limit {
                    return Err(ReadlinesError::LimitOverflow);
                }
                let enc: *const Encoding = self.encoding as *const Encoding;
                let line = if enc == UTF_8 {
                    str::from_utf8(&self.buff.split_to(ind + 1))
                        .map_err(|_| ReadlinesError::EncodingError)?
                        .to_owned()
                } else {
                    self.encoding
                        .decode(&self.buff.split_to(ind + 1), DecoderTrap::Strict)
                        .map_err(|_| ReadlinesError::EncodingError)?
                };
                return Ok(Async::Ready(Some(line)));
            }
            self.checked_buff = true;
        }
        // poll req for more bytes
        match self.stream.poll() {
            Ok(Async::Ready(Some(mut bytes))) => {
                // check if there is a newline in bytes
                let mut found: Option<usize> = None;
                for (ind, b) in bytes.iter().enumerate() {
                    if *b == b'\n' {
                        found = Some(ind);
                        break;
                    }
                }
                if let Some(ind) = found {
                    // check if line is longer than limit
                    if ind + 1 > self.limit {
                        return Err(ReadlinesError::LimitOverflow);
                    }
                    let enc: *const Encoding = self.encoding as *const Encoding;
                    let line = if enc == UTF_8 {
                        str::from_utf8(&bytes.split_to(ind + 1))
                            .map_err(|_| ReadlinesError::EncodingError)?
                            .to_owned()
                    } else {
                        self.encoding
                            .decode(&bytes.split_to(ind + 1), DecoderTrap::Strict)
                            .map_err(|_| ReadlinesError::EncodingError)?
                    };
                    // extend buffer with rest of the bytes;
                    self.buff.extend_from_slice(&bytes);
                    self.checked_buff = false;
                    return Ok(Async::Ready(Some(line)));
                }
                self.buff.extend_from_slice(&bytes);
                Ok(Async::NotReady)
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(None)) => {
                if self.buff.is_empty() {
                    return Ok(Async::Ready(None));
                }
                if self.buff.len() > self.limit {
                    return Err(ReadlinesError::LimitOverflow);
                }
                let enc: *const Encoding = self.encoding as *const Encoding;
                let line = if enc == UTF_8 {
                    str::from_utf8(&self.buff)
                        .map_err(|_| ReadlinesError::EncodingError)?
                        .to_owned()
                } else {
                    self.encoding
                        .decode(&self.buff, DecoderTrap::Strict)
                        .map_err(|_| ReadlinesError::EncodingError)?
                };
                self.buff.clear();
                Ok(Async::Ready(Some(line)))
            }
            Err(e) => Err(ReadlinesError::from(e)),
        }
    }
}

/// Future that resolves to a complete http message body.
pub struct MessageBody<T: HttpMessage> {
    limit: usize,
    length: Option<usize>,
    stream: Payload<T::Stream>,
    err: Option<PayloadError>,
    fut: Option<Box<Future<Item = Bytes, Error = PayloadError>>>,
}

impl<T> MessageBody<T>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError>,
{
    /// Create `MessageBody` for request.
    pub fn new(req: &mut T) -> MessageBody<T> {
        let mut len = None;
        if let Some(l) = req.headers().get(header::CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                } else {
                    return Self::err(PayloadError::UnknownLength);
                }
            } else {
                return Self::err(PayloadError::UnknownLength);
            }
        }

        MessageBody {
            stream: req.take_payload(),
            limit: 262_144,
            length: len,
            fut: None,
            err: None,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    fn err(e: PayloadError) -> Self {
        MessageBody {
            stream: Payload::None,
            limit: 262_144,
            fut: None,
            err: Some(e),
            length: None,
        }
    }
}

impl<T> Future for MessageBody<T>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError> + 'static,
{
    type Item = Bytes;
    type Error = PayloadError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut {
            return fut.poll();
        }

        if let Some(err) = self.err.take() {
            return Err(err);
        }

        if let Some(len) = self.length.take() {
            if len > self.limit {
                return Err(PayloadError::Overflow);
            }
        }

        // future
        let limit = self.limit;
        self.fut = Some(Box::new(
            std::mem::replace(&mut self.stream, Payload::None)
                .from_err()
                .fold(BytesMut::with_capacity(8192), move |mut body, chunk| {
                    if (body.len() + chunk.len()) > limit {
                        Err(PayloadError::Overflow)
                    } else {
                        body.extend_from_slice(&chunk);
                        Ok(body)
                    }
                })
                .map(|body| body.freeze()),
        ));
        self.poll()
    }
}

/// Future that resolves to a parsed urlencoded values.
pub struct UrlEncoded<T: HttpMessage, U> {
    stream: Payload<T::Stream>,
    limit: usize,
    length: Option<usize>,
    encoding: EncodingRef,
    err: Option<UrlencodedError>,
    fut: Option<Box<Future<Item = U, Error = UrlencodedError>>>,
}

impl<T, U> UrlEncoded<T, U>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError>,
{
    /// Create a new future to URL encode a request
    pub fn new(req: &mut T) -> UrlEncoded<T, U> {
        // check content type
        if req.content_type().to_lowercase() != "application/x-www-form-urlencoded" {
            return Self::err(UrlencodedError::ContentType);
        }
        let encoding = match req.encoding() {
            Ok(enc) => enc,
            Err(_) => return Self::err(UrlencodedError::ContentType),
        };

        let mut len = None;
        if let Some(l) = req.headers().get(header::CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                } else {
                    return Self::err(UrlencodedError::UnknownLength);
                }
            } else {
                return Self::err(UrlencodedError::UnknownLength);
            }
        };

        UrlEncoded {
            encoding,
            stream: req.take_payload(),
            limit: 262_144,
            length: len,
            fut: None,
            err: None,
        }
    }

    fn err(e: UrlencodedError) -> Self {
        UrlEncoded {
            stream: Payload::None,
            limit: 262_144,
            fut: None,
            err: Some(e),
            length: None,
            encoding: UTF_8,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl<T, U> Future for UrlEncoded<T, U>
where
    T: HttpMessage,
    T::Stream: Stream<Item = Bytes, Error = PayloadError> + 'static,
    U: DeserializeOwned + 'static,
{
    type Item = U;
    type Error = UrlencodedError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut {
            return fut.poll();
        }

        if let Some(err) = self.err.take() {
            return Err(err);
        }

        // payload size
        let limit = self.limit;
        if let Some(len) = self.length.take() {
            if len > limit {
                return Err(UrlencodedError::Overflow);
            }
        }

        // future
        let encoding = self.encoding;
        let fut = std::mem::replace(&mut self.stream, Payload::None)
            .from_err()
            .fold(BytesMut::with_capacity(8192), move |mut body, chunk| {
                if (body.len() + chunk.len()) > limit {
                    Err(UrlencodedError::Overflow)
                } else {
                    body.extend_from_slice(&chunk);
                    Ok(body)
                }
            })
            .and_then(move |body| {
                if (encoding as *const Encoding) == UTF_8 {
                    serde_urlencoded::from_bytes::<U>(&body)
                        .map_err(|_| UrlencodedError::Parse)
                } else {
                    let body = encoding
                        .decode(&body, DecoderTrap::Strict)
                        .map_err(|_| UrlencodedError::Parse)?;
                    serde_urlencoded::from_str::<U>(&body)
                        .map_err(|_| UrlencodedError::Parse)
                }
            });
        self.fut = Some(Box::new(fut));
        self.poll()
    }
}

#[cfg(test)]
mod tests {
    use encoding::all::ISO_8859_2;
    use encoding::Encoding;
    use futures::Async;
    use mime;
    use serde_derive::Deserialize;

    use super::*;
    use crate::test::TestRequest;

    #[test]
    fn test_content_type() {
        let req = TestRequest::with_header("content-type", "text/plain").finish();
        assert_eq!(req.content_type(), "text/plain");
        let req =
            TestRequest::with_header("content-type", "application/json; charset=utf=8")
                .finish();
        assert_eq!(req.content_type(), "application/json");
        let req = TestRequest::default().finish();
        assert_eq!(req.content_type(), "");
    }

    #[test]
    fn test_mime_type() {
        let req = TestRequest::with_header("content-type", "application/json").finish();
        assert_eq!(req.mime_type().unwrap(), Some(mime::APPLICATION_JSON));
        let req = TestRequest::default().finish();
        assert_eq!(req.mime_type().unwrap(), None);
        let req =
            TestRequest::with_header("content-type", "application/json; charset=utf-8")
                .finish();
        let mt = req.mime_type().unwrap().unwrap();
        assert_eq!(mt.get_param(mime::CHARSET), Some(mime::UTF_8));
        assert_eq!(mt.type_(), mime::APPLICATION);
        assert_eq!(mt.subtype(), mime::JSON);
    }

    #[test]
    fn test_mime_type_error() {
        let req = TestRequest::with_header(
            "content-type",
            "applicationadfadsfasdflknadsfklnadsfjson",
        )
        .finish();
        assert_eq!(Err(ContentTypeError::ParseError), req.mime_type());
    }

    #[test]
    fn test_encoding() {
        let req = TestRequest::default().finish();
        assert_eq!(UTF_8.name(), req.encoding().unwrap().name());

        let req = TestRequest::with_header("content-type", "application/json").finish();
        assert_eq!(UTF_8.name(), req.encoding().unwrap().name());

        let req = TestRequest::with_header(
            "content-type",
            "application/json; charset=ISO-8859-2",
        )
        .finish();
        assert_eq!(ISO_8859_2.name(), req.encoding().unwrap().name());
    }

    #[test]
    fn test_encoding_error() {
        let req = TestRequest::with_header("content-type", "applicatjson").finish();
        assert_eq!(Some(ContentTypeError::ParseError), req.encoding().err());

        let req = TestRequest::with_header(
            "content-type",
            "application/json; charset=kkkttktk",
        )
        .finish();
        assert_eq!(
            Some(ContentTypeError::UnknownEncoding),
            req.encoding().err()
        );
    }

    #[test]
    fn test_chunked() {
        let req = TestRequest::default().finish();
        assert!(!req.chunked().unwrap());

        let req =
            TestRequest::with_header(header::TRANSFER_ENCODING, "chunked").finish();
        assert!(req.chunked().unwrap());

        let req = TestRequest::default()
            .header(
                header::TRANSFER_ENCODING,
                Bytes::from_static(b"some va\xadscc\xacas0xsdasdlue"),
            )
            .finish();
        assert!(req.chunked().is_err());
    }

    impl PartialEq for UrlencodedError {
        fn eq(&self, other: &UrlencodedError) -> bool {
            match *self {
                UrlencodedError::Chunked => match *other {
                    UrlencodedError::Chunked => true,
                    _ => false,
                },
                UrlencodedError::Overflow => match *other {
                    UrlencodedError::Overflow => true,
                    _ => false,
                },
                UrlencodedError::UnknownLength => match *other {
                    UrlencodedError::UnknownLength => true,
                    _ => false,
                },
                UrlencodedError::ContentType => match *other {
                    UrlencodedError::ContentType => true,
                    _ => false,
                },
                _ => false,
            }
        }
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Info {
        hello: String,
    }

    #[test]
    fn test_urlencoded_error() {
        let mut req = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "xxxx")
        .finish();
        assert_eq!(
            req.urlencoded::<Info>().poll().err().unwrap(),
            UrlencodedError::UnknownLength
        );

        let mut req = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "1000000")
        .finish();
        assert_eq!(
            req.urlencoded::<Info>().poll().err().unwrap(),
            UrlencodedError::Overflow
        );

        let mut req = TestRequest::with_header(header::CONTENT_TYPE, "text/plain")
            .header(header::CONTENT_LENGTH, "10")
            .finish();
        assert_eq!(
            req.urlencoded::<Info>().poll().err().unwrap(),
            UrlencodedError::ContentType
        );
    }

    #[test]
    fn test_urlencoded() {
        let mut req = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(header::CONTENT_LENGTH, "11")
        .set_payload(Bytes::from_static(b"hello=world"))
        .finish();

        let result = req.urlencoded::<Info>().poll().ok().unwrap();
        assert_eq!(
            result,
            Async::Ready(Info {
                hello: "world".to_owned()
            })
        );

        let mut req = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8",
        )
        .header(header::CONTENT_LENGTH, "11")
        .set_payload(Bytes::from_static(b"hello=world"))
        .finish();

        let result = req.urlencoded().poll().ok().unwrap();
        assert_eq!(
            result,
            Async::Ready(Info {
                hello: "world".to_owned()
            })
        );
    }

    #[test]
    fn test_message_body() {
        let mut req = TestRequest::with_header(header::CONTENT_LENGTH, "xxxx").finish();
        match req.body().poll().err().unwrap() {
            PayloadError::UnknownLength => (),
            _ => unreachable!("error"),
        }

        let mut req =
            TestRequest::with_header(header::CONTENT_LENGTH, "1000000").finish();
        match req.body().poll().err().unwrap() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }

        let mut req = TestRequest::default()
            .set_payload(Bytes::from_static(b"test"))
            .finish();
        match req.body().poll().ok().unwrap() {
            Async::Ready(bytes) => assert_eq!(bytes, Bytes::from_static(b"test")),
            _ => unreachable!("error"),
        }

        let mut req = TestRequest::default()
            .set_payload(Bytes::from_static(b"11111111111111"))
            .finish();
        match req.body().limit(5).poll().err().unwrap() {
            PayloadError::Overflow => (),
            _ => unreachable!("error"),
        }
    }

    #[test]
    fn test_readlines() {
        let mut req = TestRequest::default()
            .set_payload(Bytes::from_static(
                b"Lorem Ipsum is simply dummy text of the printing and typesetting\n\
                  industry. Lorem Ipsum has been the industry's standard dummy\n\
                  Contrary to popular belief, Lorem Ipsum is not simply random text.",
            ))
            .finish();
        let mut r = Readlines::new(&mut req);
        match r.poll().ok().unwrap() {
            Async::Ready(Some(s)) => assert_eq!(
                s,
                "Lorem Ipsum is simply dummy text of the printing and typesetting\n"
            ),
            _ => unreachable!("error"),
        }
        match r.poll().ok().unwrap() {
            Async::Ready(Some(s)) => assert_eq!(
                s,
                "industry. Lorem Ipsum has been the industry's standard dummy\n"
            ),
            _ => unreachable!("error"),
        }
        match r.poll().ok().unwrap() {
            Async::Ready(Some(s)) => assert_eq!(
                s,
                "Contrary to popular belief, Lorem Ipsum is not simply random text."
            ),
            _ => unreachable!("error"),
        }
    }
}
