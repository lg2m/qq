//! Shared loopback HTTP fixtures for provider interface tests.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) struct CapturedRequest {
    wire: String,
}

impl CapturedRequest {
    pub(crate) fn request_line(&self) -> Option<&str> {
        self.head().lines().next()
    }

    pub(crate) fn header(&self, expected_name: &str) -> Option<&str> {
        self.head()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(expected_name))
            .map(|(_, value)| value.trim())
    }

    pub(crate) fn json_body(&self) -> serde_json::Value {
        serde_json::from_str(self.body()).expect("captured request body must be JSON")
    }

    fn head(&self) -> &str {
        self.wire
            .split_once("\r\n\r\n")
            .expect("captured request must contain an HTTP head")
            .0
    }

    fn body(&self) -> &str {
        self.wire
            .split_once("\r\n\r\n")
            .expect("captured request must contain an HTTP body separator")
            .1
    }
}

pub(crate) struct LoopbackServer {
    pub(crate) base_url: String,
    request: JoinHandle<CapturedRequest>,
}

impl LoopbackServer {
    pub(crate) fn sse(body: impl Into<String>) -> Self {
        Self::respond(200, "text/event-stream", body)
    }

    pub(crate) fn respond(
        status: u16,
        content_type: &'static str,
        body: impl Into<String>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener must bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let body = body.into();
        let request = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("loopback request must connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("loopback read timeout must be configurable");
            let request = read_request(&mut stream);
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("loopback response must be written");
            CapturedRequest {
                wire: String::from_utf8(request).expect("captured request must be UTF-8"),
            }
        });
        Self { base_url, request }
    }

    pub(crate) fn capture(self) -> CapturedRequest {
        self.request.join().expect("loopback server must not panic")
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0; 4_096];
    loop {
        let read = stream.read(&mut buffer).expect("request read must succeed");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        if request.len() >= body_start + content_length {
            break;
        }
    }
    request
}
