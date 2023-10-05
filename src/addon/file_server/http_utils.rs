use std::io::SeekFrom;
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use futures::Stream;
use http::response::Builder as HttpResponseBuilder;
use hyper::body::Body;
use hyper::body::Bytes;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{self, Poll};
use http::HeaderValue;
use tokio::io::{AsyncRead, AsyncSeekExt, ReadBuf};

use super::file::File;

const FILE_BUFFER_SIZE: usize = 8 * 1024;

pub type FileBuffer = Box<[MaybeUninit<u8>; FILE_BUFFER_SIZE]>;

/// HTTP Response `Cache-Control` directive
///
/// Allow dead code until we have support for cache control configuration
#[allow(dead_code)]

pub enum CacheControlDirective {
    /// Cache-Control: must-revalidate
    MustRevalidate,
    /// Cache-Control: no-cache
    NoCache,
    /// Cache-Control: no-store
    NoStore,
    /// Cache-Control: no-transform
    NoTransform,
    /// Cache-Control: public
    Public,
    /// Cache-Control: private
    Private,
    /// Cache-Control: proxy-revalidate
    ProxyRavalidate,
    /// Cache-Control: max-age=<seconds>
    MaxAge(u64),
    /// Cache-Control: s-maxage=<seconds>
    SMaxAge(u64),
}

impl ToString for CacheControlDirective {
    fn to_string(&self) -> String {
        match &self {
            Self::MustRevalidate => String::from("must-revalidate"),
            Self::NoCache => String::from("no-cache"),
            Self::NoStore => String::from("no-store"),
            Self::NoTransform => String::from("no-transform"),
            Self::Public => String::from("public"),
            Self::Private => String::from("private"),
            Self::ProxyRavalidate => String::from("proxy-revalidate"),
            Self::MaxAge(age) => format!("max-age={}", age),
            Self::SMaxAge(age) => format!("s-maxage={}", age),
        }
    }
}

pub struct ResponseHeaders {
    cache_control: String,
    content_length: u64,
    content_type: String,
    etag: String,
    last_modified: String,
}

impl ResponseHeaders {
    pub fn new(
        file: &File,
        cache_control_directive: CacheControlDirective,
        range: Option<(usize, usize)>
    ) -> Result<ResponseHeaders> {
        let last_modified = file.last_modified()?;

        Ok(ResponseHeaders {
            cache_control: cache_control_directive.to_string(),
            content_length: ResponseHeaders::content_length(file, range),
            content_type: ResponseHeaders::content_type(file),
            etag: ResponseHeaders::etag(file, &last_modified),
            last_modified: ResponseHeaders::last_modified(&last_modified),
        })
    }

    fn content_length(file: &File, range: Option<(usize, usize)>) -> u64 {
        if let Some((first_byte, last_byte)) = range {
            if last_byte == u64::MAX as usize {
                return file.size() - first_byte as u64;
            }
            last_byte.abs_diff(first_byte) as u64 + 1
        } else {
            file.size()
        }
    }

    fn content_type(file: &File) -> String {
        file.mime().to_string()
    }

    fn etag(file: &File, last_modified: &DateTime<Local>) -> String {
        format!(
            "W/\"{0:x}-{1:x}.{2:x}\"",
            file.size(),
            last_modified.timestamp(),
            last_modified.timestamp_subsec_nanos(),
        )
    }

    fn last_modified(last_modified: &DateTime<Local>) -> String {
        format!(
            "{} GMT",
            last_modified
                .with_timezone(&Utc)
                .format("%a, %e %b %Y %H:%M:%S")
        )
    }
}

pub async fn make_http_file_response(
    file: File,
    cache_control_directive: CacheControlDirective,
    range: Option<(usize, usize)>
) -> Result<hyper::http::Response<Body>> {
    let filesize = file.size();
    let headers = ResponseHeaders::new(&file, cache_control_directive, range)?;
    let mut builder = HttpResponseBuilder::new()
        .header(http::header::CONTENT_LENGTH, headers.content_length)
        .header(http::header::CACHE_CONTROL, headers.cache_control)
        .header(http::header::CONTENT_TYPE, headers.content_type)
        .header(http::header::ETAG, headers.etag)
        .header(http::header::LAST_MODIFIED, headers.last_modified);

    let body = file_bytes_into_http_body(file, range).await;

    if let Some((first_byte, last_byte)) = range {
        let mut last_byte_str = format!("{filesize}");
        if last_byte != u64::MAX as usize {
            last_byte_str = format!("{last_byte}");
        }
        builder = builder
            .header(http::header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {first_byte}-{last_byte_str}/{filesize}")).unwrap())
            .status(http::status::StatusCode::PARTIAL_CONTENT);
    }

    let response = builder
        .body(body)
        .context("Failed to build HTTP File Response")?;

    Ok(response)
}

pub async fn file_bytes_into_http_body(mut file: File, range: Option<(usize, usize)>) -> Body {
    let mut position = 0u64;
    if let Some((first_byte, _)) = range {
        position = file.file.seek(SeekFrom::Start(first_byte as u64)).await.unwrap();

    }

    let byte_stream = ByteStream {
        file: file.file,
        buffer: Box::new([MaybeUninit::uninit(); FILE_BUFFER_SIZE]),
        range,
        position,
    };

    Body::wrap_stream(byte_stream)
}

pub struct ByteStream {
    file: tokio::fs::File,
    buffer: FileBuffer,
    range: Option<(usize, usize)>,
    position: u64,
}

impl Stream for ByteStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        poll_read(cx, &mut self)
    }
}

fn poll_read(cx: &mut task::Context<'_>, stream: &mut ByteStream) -> Poll<Option<<ByteStream as futures::Stream>::Item>> {

    let mut buff_read_len = stream.buffer.len();
    if let Some((_, last_byte)) = stream.range {
        if last_byte != u64::MAX as usize {
            buff_read_len = std::cmp::min(buff_read_len, (stream.position.abs_diff(last_byte as u64) + 1) as usize)
        }
    }
    let mut read_buffer = ReadBuf::uninit(&mut stream.buffer[..buff_read_len]);

    match Pin::new(&mut stream.file).poll_read(cx, &mut read_buffer) {
        Poll::Ready(Ok(())) => {
            let filled = read_buffer.filled();
            stream.position += filled.len() as u64;
            if filled.is_empty() {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(Ok(Bytes::copy_from_slice(filled))))
            }
        }
        Poll::Ready(Err(error)) => Poll::Ready(Some(Err(error.into()))),
        Poll::Pending => Poll::Pending,
    }
}
