//! Shared browser-OAuth helpers used by [`crate::reauth`] (the automatic
//! re-auth on `invalid_grant`) and [`crate::login`] (the `login` subcommands):
//! a minimal localhost callback server, percent-encoding, and opening a URL in
//! the default browser.

use tracing::warn;

/// Accept a single connection on `listener`, parse the OAuth redirect, and
/// return the `code` query parameter. Responds to the browser with a small
/// success/error page. Works regardless of the callback path.
pub async fn accept_oauth_callback(
    listener: &tokio::net::TcpListener,
) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (mut stream, _) = listener.accept().await?;
    // Read exactly the HTTP request line. A single `read()` can return a partial
    // request if the browser's bytes are segmented across TCP reads; `read_line`
    // buffers until the newline. Scope the reader so its `&mut` borrow on `stream`
    // is released before we write the response below.
    let path = {
        let mut reader = tokio::io::BufReader::new(&mut stream);
        let mut first_line = String::new();
        reader.read_line(&mut first_line).await?;
        first_line.split_whitespace().nth(1).unwrap_or("").to_string()
    };

    if let Some(error) = extract_query_param(&path, "error") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
             <html><body><h2>Authentication Failed</h2>\
             <p>Error: {}</p>\
             <p>You can close this window.</p></body></html>",
            error
        );
        stream.write_all(response.as_bytes()).await?;
        anyhow::bail!("OAuth error: {}", error);
    }

    match extract_query_param(&path, "code") {
        Some(code) => {
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                 <html><body><h2>Authentication Successful!</h2>\
                 <p>You can close this window and return to your terminal.</p></body></html>";
            stream.write_all(response.as_bytes()).await?;
            Ok(code)
        }
        None => {
            let response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
                 <html><body><h2>Error</h2>\
                 <p>No authorization code received.</p></body></html>";
            stream.write_all(response.as_bytes()).await?;
            anyhow::bail!("No authorization code in callback");
        }
    }
}

/// Open `url` in the default browser (macOS `open`).
pub fn open_browser(url: &str) {
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        warn!("Failed to open browser: {}", e);
    }
}

fn extract_query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split('?').nth(1)?;
    query.split('&').find_map(|param| {
        let mut parts = param.splitn(2, '=');
        let k = parts.next()?;
        let v = parts.next()?;
        if k == key {
            Some(percent_decode(v))
        } else {
            None
        }
    })
}

pub fn percent_encode(input: &str) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

pub fn percent_decode(input: &str) -> String {
    // Decode at the byte level, then validate once. Percent-escapes encode UTF-8
    // bytes, so a multi-byte character spans several `%XX` pairs — collecting
    // decoded bytes (rather than pushing each as a `char`) keeps it intact. Hex
    // pairs are decoded straight from the byte stream, with no temporary
    // allocation per escape.
    let mut bytes: Vec<u8> = Vec::with_capacity(input.len());
    let mut iter = input.bytes();
    while let Some(b) = iter.next() {
        match b {
            b'%' => {
                let (h1, h2) = (iter.next(), iter.next());
                let decoded = match (h1, h2) {
                    (Some(d1), Some(d2)) => {
                        (d1 as char).to_digit(16).zip((d2 as char).to_digit(16))
                    }
                    _ => None,
                };
                match decoded {
                    Some((v1, v2)) => bytes.push((v1 << 4 | v2) as u8),
                    // Malformed escape — preserve the literal characters.
                    None => {
                        bytes.push(b'%');
                        bytes.extend(h1);
                        bytes.extend(h2);
                    }
                }
            }
            b'+' => bytes.push(b' '),
            // Any other source byte (incl. UTF-8 continuation bytes) passes through.
            other => bytes.push(other),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
