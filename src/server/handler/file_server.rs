use async_trait::async_trait;
use http::response::Builder as HttpResponseBuilder;
use http::{header, HeaderValue, StatusCode};
use hyper::{Body, Method, Request};
use std::sync::Arc;
use regex::Regex;
use tokio::sync::Mutex;
use lazy_static::lazy_static;

use crate::addon::file_server::FileServer;
use crate::utils::error::make_http_error_response;

use super::RequestHandler;

lazy_static! {
    static ref BYTES_RE: Regex = Regex::new(r"bytes=(\d+)-(\d+)?").unwrap();
}

pub struct FileServerHandler {
    file_server: Arc<FileServer>,
}

impl FileServerHandler {
    pub fn new(file_server: FileServer) -> Self {
        let file_server = Arc::new(file_server);

        FileServerHandler { file_server }
    }
}

#[async_trait]
impl RequestHandler for FileServerHandler {
    async fn handle(&self, req: Arc<Mutex<Request<Body>>>) -> Arc<Mutex<http::Response<Body>>> {
        let request_lock = req.lock().await;
        let req_path = request_lock.uri().to_string();
        let req_method = request_lock.method();

        return match *req_method {
            Method::GET => {
                if let Some(range_header) = request_lock.headers().get(header::RANGE) {
                    let byte_range = range_header.to_str().unwrap();
                    return if let Some(caps) = BYTES_RE.captures(byte_range) {
                        let first_byte = caps.get(1).unwrap().as_str().parse().unwrap();
                        let mut last_byte = usize::MAX;
                        if let Some(las_byte_s) = caps.get(2) {
                            last_byte = las_byte_s.as_str().parse().unwrap();
                        }
                        let response = self.file_server.resolve(req_path, Some((first_byte, last_byte)))
                            .await.unwrap();
                        Arc::new(Mutex::new(response))
                    } else {
                        let response = make_http_error_response(StatusCode::BAD_REQUEST,
                                                                "invalid byte range");
                        Arc::new(Mutex::new(response))
                    }
                }
                let response = self.file_server.resolve(req_path, None).await.unwrap();
                Arc::new(Mutex::new(response))
            }
            Method::OPTIONS => {
                let response = HttpResponseBuilder::new()
                    .status(StatusCode::OK)
                    .header(header::ACCEPT_RANGES, HeaderValue::from_str("bytes").unwrap())
                    .body(Body::empty())
                    .expect("Failed to build response");
                Arc::new(Mutex::new(response))
            }
            _ => {
                Arc::new(Mutex::new(
                    HttpResponseBuilder::new()
                        .status(StatusCode::METHOD_NOT_ALLOWED)
                        .body(Body::empty())
                        .expect("Unable to build response"),
                ))
            }
        }
    }
}
