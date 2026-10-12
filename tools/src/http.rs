// SPDX-License-Identifier: MIT
//! A minimal HTTP/1.1 client for the loopback service checks. Every request
//! uses its own connection, and headers are sent exactly as given.
use crate::util::{Failure, Result, require};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    time::Duration,
};

pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
impl Response {
    /// The body of a JSON response; anything else is `Null`.
    pub fn json(&self) -> Value {
        let json = self
            .headers
            .get("content-type")
            .is_some_and(|kind| kind.starts_with("application/json"));
        json.then(|| serde_json::from_slice(&self.body).ok())
            .flatten()
            .unwrap_or(Value::Null)
    }
}

/// Sends one request. A `Host` or `Content-Length` header in `headers`
/// replaces the default; repeated names are sent repeatedly.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Response> {
    let failed = |error: std::io::Error| Failure(format!("{method} {path}: {error}"));
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(failed)?;
    stream.set_read_timeout(Some(timeout)).map_err(failed)?;
    stream.set_write_timeout(Some(timeout)).map_err(failed)?;
    let supplied = |name: &str| {
        headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    };
    let mut text = format!("{method} {path} HTTP/1.1\r\n");
    if !supplied("Host") {
        text.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    text.push_str("Accept-Encoding: identity\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body.filter(|_| !supplied("Content-Length")) {
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("\r\n");
    stream.write_all(text.as_bytes()).map_err(failed)?;
    if let Some(body) = body {
        // The service may answer and close before an oversized body is sent.
        let _ = stream.write_all(body);
    }
    let mut raw = Vec::new();
    let mut buffer = [0; 16384];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => raw.extend_from_slice(&buffer[..count]),
            // A reset after a complete early answer still leaves that answer.
            Err(_) if raw.windows(4).any(|window| window == b"\r\n\r\n") => break,
            Err(error) => return Err(failed(error)),
        }
    }
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| Failure(format!("{method} {path}: incomplete response")))?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| Failure(format!("{method} {path}: invalid status line")))?;
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        body = dechunk(&body)?;
    } else if let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        require(
            body.len() >= length,
            format!("{method} {path}: truncated response"),
        )?;
        body.truncate(length);
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>> {
    let invalid = || Failure("Invalid chunked response".into());
    let mut body = Vec::new();
    loop {
        let line = raw
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(invalid)?;
        let size = std::str::from_utf8(&raw[..line])
            .ok()
            .and_then(|text| {
                usize::from_str_radix(text.split(';').next().unwrap_or_default().trim(), 16).ok()
            })
            .ok_or_else(invalid)?;
        raw = &raw[line + 2..];
        if size == 0 {
            return Ok(body);
        }
        require(raw.len() >= size + 2, "Invalid chunked response")?;
        body.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use std::{
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
    };

    /// A local service that answers every request through `answer`, which
    /// receives the method, the path and the request headers.
    pub struct Service {
        pub port: u16,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }
    pub type Headers = BTreeMap<String, Vec<String>>;
    impl Service {
        pub fn start(
            answer: impl Fn(&str, &str, &Headers, &[u8]) -> (u16, Value) + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
            let port = listener.local_addr().expect("address").port();
            listener.set_nonblocking(true).expect("nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = std::thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    stream.set_nonblocking(false).expect("blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("timeout");
                    let mut raw = Vec::new();
                    let mut buffer = [0; 4096];
                    let (head, mut body) = loop {
                        let Ok(count) = stream.read(&mut buffer) else {
                            break (String::new(), Vec::new());
                        };
                        raw.extend_from_slice(&buffer[..count]);
                        if let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            break (
                                String::from_utf8_lossy(&raw[..split]).into_owned(),
                                raw[split + 4..].to_vec(),
                            );
                        }
                        if count == 0 {
                            break (String::new(), Vec::new());
                        }
                    };
                    let mut lines = head.split("\r\n");
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let (method, path) = (
                        first.next().unwrap_or_default(),
                        first.next().unwrap_or_default(),
                    );
                    let mut headers = Headers::new();
                    for (name, value) in lines.filter_map(|line| line.split_once(':')) {
                        headers
                            .entry(name.trim().to_lowercase())
                            .or_default()
                            .push(value.trim().into());
                    }
                    let length = headers
                        .get("content-length")
                        .and_then(|values| values[0].parse::<usize>().ok())
                        .unwrap_or(0);
                    while body.len() < length {
                        let Ok(count) = stream.read(&mut buffer) else {
                            break;
                        };
                        if count == 0 {
                            break;
                        }
                        body.extend_from_slice(&buffer[..count]);
                    }
                    let (status, value) = answer(method, path, &headers, &body);
                    let payload = value.to_string();
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    );
                }
            });
            Self {
                port,
                stop,
                worker: Some(worker),
            }
        }
    }
    impl Drop for Service {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::Service, *};
    use serde_json::json;

    #[test]
    fn headers_are_sent_as_given_and_json_bodies_are_decoded() {
        let service = Service::start(|method, path, headers, body| {
            (
                201,
                json!({"method": method, "path": path, "host": headers.get("host"),
                    "tokens": headers.get("x-flightdeck-token"), "length": headers.get("content-length"),
                    "body": String::from_utf8_lossy(body)}),
            )
        });
        let timeout = Duration::from_secs(5);
        let plain =
            request(service.port, "GET", "/api/status", &[], None, timeout).expect("response");
        assert_eq!(plain.status, 201);
        assert_eq!(plain.headers["content-type"], "application/json");
        let value = plain.json();
        assert_eq!(value["method"], "GET");
        assert_eq!(
            value["host"],
            json!([format!("127.0.0.1:{}", service.port)])
        );
        assert_eq!(value["length"], Value::Null);
        let sent = request(
            service.port,
            "POST",
            "/api/config",
            &[
                ("Host", "example.invalid"),
                ("X-Flightdeck-Token", "a"),
                ("X-Flightdeck-Token", "b"),
            ],
            Some(b"{}"),
            timeout,
        )
        .expect("response")
        .json();
        assert_eq!(sent["host"], json!(["example.invalid"]));
        assert_eq!(sent["tokens"], json!(["a", "b"]));
        assert_eq!(sent["length"], json!(["2"]));
        assert_eq!(sent["body"], "{}");
    }

    #[test]
    fn chunked_bodies_and_refused_connections_are_handled() {
        assert_eq!(
            dechunk(b"4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n").expect("chunks"),
            b"Wikipedia"
        );
        assert!(dechunk(b"4\r\nWi").is_err());
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        assert!(request(port, "GET", "/", &[], None, Duration::from_millis(200)).is_err());
    }
}
