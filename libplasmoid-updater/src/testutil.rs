// SPDX-License-Identifier: GPL-3.0-or-later

//! Test-only harness: the request-reading HTTP server the API, store and
//! registry test modules answer their requests with, and the log capture the
//! installer and checker tests read back.

pub(crate) mod logs;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One response: status, extra headers, body.
pub(crate) type Reply = (u16, Vec<(&'static str, String)>, String);

/// A `200 OK` carrying `body`.
pub(crate) fn ok(body: impl Into<String>) -> Reply {
    (200, Vec::new(), body.into())
}

/// A `404 Not Found` with an empty body.
pub(crate) fn not_found() -> Reply {
    (404, Vec::new(), String::new())
}

/// A `302 Found` pointing at `location`.
pub(crate) fn redirect(location: &str) -> Reply {
    (302, vec![("Location", location.to_string())], String::new())
}

/// Serves the response `respond` picks for a request target, one connection
/// per request, and counts the requests. Returns the `/ocs/v1` base URL.
pub(crate) fn serve(
    respond: impl Fn(&str) -> Reply + Send + Sync + 'static,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&requests);
    let respond = Arc::new(respond);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let respond = Arc::clone(&respond);
            let count = Arc::clone(&count);
            std::thread::spawn(move || answer_request(stream, respond.as_ref(), &count));
        }
    });
    (format!("http://127.0.0.1:{port}/ocs/v1"), requests)
}

fn answer_request<F: Fn(&str) -> Reply>(mut stream: TcpStream, respond: &F, count: &AtomicUsize) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
        }
    }
    count.fetch_add(1, Ordering::SeqCst);
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let (status, headers, body) = respond(target);
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n",
        reason(status),
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Unknown",
    }
}
