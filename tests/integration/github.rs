//! An in-process fake GitHub, for driving `bastion github report` against a real
//! HTTP server with no network. It accepts connections, records each request, and
//! replies with a small canned JSON object.

use std::time::Duration;

/// One captured HTTP request to the fake GitHub.
#[derive(Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: String,
}

/// A minimal in-process GitHub: it accepts connections, records each request, and
/// replies with a small JSON object (an empty comment list for `GET`, a created id
/// for `POST`), closing the connection each time so the client opens a fresh one.
///
/// The accept loop is non-blocking and watches a stop flag, so the server can never
/// hang the test waiting for a request the binary did not make: the test runs the
/// binary to completion, then flips the flag and joins.
pub(crate) struct FakeGitHub {
    pub(crate) url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<Vec<CapturedRequest>>,
}

impl FakeGitHub {
    pub(crate) fn start() -> Self {
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake github");
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();

        let handle = std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            let mut recorded = Vec::new();
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(req) = serve_one(stream) {
                            recorded.push(req);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("fake github accept failed: {e}"),
                }
            }
            recorded
        });

        Self { url, stop, handle }
    }

    /// Stop the server and return everything it recorded.
    pub(crate) fn finish(self) -> Vec<CapturedRequest> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle.join().expect("fake github thread")
    }
}

/// Read one HTTP request off `stream`, reply, and return what was captured.
fn serve_one(mut stream: std::net::TcpStream) -> Option<CapturedRequest> {
    use std::io::{Read, Write};

    // The listener is non-blocking, and on Windows an accepted socket inherits that
    // mode (POSIX does not, so this only bites on Windows). A non-blocking read would
    // return `WouldBlock` whenever the request bytes have not landed the instant we
    // call `read`, and `serve_one`'s `.ok()?` would then silently drop the request.
    // Force blocking mode so the read timeout below actually governs the wait.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    // Read until the header terminator, then drain the declared body.
    let header_end = loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buf.len() - body_start < content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let body = String::from_utf8_lossy(&buf[body_start..]).into_owned();

    let (status, json) = if method == "GET" {
        (200, get_response(&path))
    } else if path.ends_with("/check-runs") {
        // GitHub stamps the creating app on every check run. With the default
        // GITHUB_TOKEN that app is `github-actions`, which the report reads back to
        // detect that no dedicated app is configured.
        (
            201,
            r#"{"id":1,"app":{"slug":"github-actions"}}"#.to_string(),
        )
    } else {
        (201, r#"{"id":1}"#.to_string())
    };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
        json.len()
    );
    stream.write_all(response.as_bytes()).ok()?;
    stream.flush().ok();

    Some(CapturedRequest { method, path, body })
}

/// The canned JSON body for a `GET` request, branched on its path.
///
/// The review CI path (`bastion review --repo --pr`, with `attestations: true`)
/// issues GETs beyond the report path's comment listing: the pull request itself
/// (for its body, author login, and head SHA -- see `src/github/context.rs`
/// `gather`) and the PR author's registered SSH signing keys. Both need a shaped
/// body rather than the generic empty-list default so the attestation lookup has
/// something to parse; every other GET (the comment listings) still gets `[]`,
/// which `gather` and the reporter both tolerate as "nothing there".
///
/// The PR response uses local git refs in its SHA fields. They remain resolvable
/// in every throwaway repository while exercising the same lookup paths as full
/// commit ids.
fn get_response(path: &str) -> String {
    if path.contains("/ssh_signing_keys") {
        // No keys registered: the CI attestation path treats this the same as any
        // other verification failure and falls back to a full run.
        "[]".to_string()
    } else if path.contains("/pulls/") && !path.contains("/comments") {
        r#"{"body":null,"user":{"login":"ada"},"head":{"ref":"feature","sha":"HEAD"},"base":{"ref":"main","sha":"main"}}"#.to_string()
    } else {
        "[]".to_string()
    }
}

/// First index of `needle` within `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl std::fmt::Debug for CapturedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.method, self.path)
    }
}
