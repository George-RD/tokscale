use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A request observed by the local HTTP server used in usage-provider tests.
#[derive(Debug, PartialEq)]
pub(super) struct Seen {
    pub(super) request: String,
    pub(super) bearer: Option<String>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Unknown",
    }
}

/// Starts a minimal HTTP/1.1 server for a provider test.
///
/// The handler receives the request path and the number of requests already
/// handled, so callers retain control of provider-specific routes and staged
/// responses. The server thread intentionally remains blocked on `accept`
/// when its test ends; the test process tears it down.
pub(super) fn spawn_server<F>(handler: F) -> (String, Arc<Mutex<Vec<Seen>>>)
where
    F: FnMut(&str, usize) -> (u16, String) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let log = Arc::new(Mutex::new(Vec::new()));
    let server_log = Arc::clone(&log);
    std::thread::spawn(move || {
        let mut handler = handler;
        let mut calls = 0usize;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            let mut head = request.split_whitespace();
            let method = head.next().unwrap_or("?");
            let path = head.next().unwrap_or("/");
            let bearer = request
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().trim_start_matches("Bearer ").to_string());
            if let Ok(mut seen) = server_log.lock() {
                seen.push(Seen {
                    request: format!("{method} {path}"),
                    bearer,
                });
            }
            let (status, body) = handler(path, calls);
            calls += 1;
            let response = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                reason(status),
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), log)
}

/// Starts a local server whose response bytes the caller writes verbatim.
///
/// [`spawn_server`] always emits a `Content-Length` that matches the body it
/// was handed, which is exactly the assumption a body-ceiling test must not
/// make: the header is optional, and a server is free to advertise a size it
/// never sends. The handler here returns the whole response — status line,
/// headers and body — so a test can promise half a gigabyte and send nothing,
/// or omit the header and stream past the ceiling.
///
/// Write errors are ignored on purpose: a client that aborts mid-transfer
/// resets the connection, and that is the behaviour under test. The thread
/// lingers briefly before dropping the socket so a client that rejects a
/// response on its headers alone is not racing a FIN.
pub(super) fn spawn_raw_server<F>(handler: F) -> String
where
    F: FnMut(&str) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw test server");
    let addr = listener.local_addr().expect("raw test server addr");
    std::thread::spawn(move || {
        let mut handler = handler;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let response = handler(&path);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    format!("http://{addr}")
}
