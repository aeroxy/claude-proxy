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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener.accept().await?;
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");

    if let Some(error) = extract_query_param(path, "error") {
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

    match extract_query_param(path, "code") {
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
    // Accumulate raw bytes, then decode once. Percent-escapes encode UTF-8
    // bytes, so a multi-byte character spans several `%XX` pairs — pushing each
    // decoded byte straight into a `String` (`byte as char`) would mangle it
    // into separate Latin-1 codepoints.
    let mut bytes: Vec<u8> = Vec::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) if hex.len() == 2 => bytes.push(byte),
                    // Malformed escape — preserve the literal characters.
                    _ => {
                        bytes.push(b'%');
                        bytes.extend_from_slice(hex.as_bytes());
                    }
                }
            }
            '+' => bytes.push(b' '),
            // Pass any other source char through as its UTF-8 bytes.
            _ => {
                let mut tmp = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
