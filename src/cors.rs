//! CORS for origin mode, so a browser page on any origin (`https://example.com`,
//! `http://localhost:3000`) can call the surfaces we serve as a plain-HTTP
//! origin. Applied by the listener in [`crate::proxy`] around every plain-HTTP
//! reply. A CONNECT tunnel is left alone: the requests inside it are addressed
//! to the real host, whose CORS is its own business.

use hyper::body::Bytes;
use hyper::header::{
    HeaderValue, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_REQUEST_HEADERS,
    ACCESS_CONTROL_REQUEST_METHOD,
};
use hyper::{Method, Request, Response, StatusCode};

use crate::proxy::{full_body, ProxyBody};

/// Answer `req` if it is a CORS preflight, allowing whatever it asks for.
///
/// The requested headers are echoed rather than answered with `*`, because the
/// wildcard never covers `Authorization` — which every OpenAI-style client sends.
pub fn preflight<B>(req: &Request<B>) -> Option<Response<ProxyBody>> {
    if req.method() != Method::OPTIONS {
        return None;
    }
    let method = req.headers().get(ACCESS_CONTROL_REQUEST_METHOD)?;
    let mut resp = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    if let Some(headers) = req.headers().get(ACCESS_CONTROL_REQUEST_HEADERS) {
        resp = resp.header(ACCESS_CONTROL_ALLOW_HEADERS, headers.clone());
    }
    Some(resp.body(full_body(Bytes::new())).unwrap())
}

/// Let a page on any origin read `resp`, response headers included — the
/// `retry-after` and rate-limit headers the passthrough surfaces forward are
/// what a client's retry logic reads.
///
/// A header the reply already carries is kept: the surfaces never set one, so
/// it can only come from a Map Local rule, whose fixture is replayed as written.
pub fn allow<B>(mut resp: Response<B>) -> Response<B> {
    let headers = resp.headers_mut();
    headers
        .entry(ACCESS_CONTROL_ALLOW_ORIGIN)
        .or_insert(HeaderValue::from_static("*"));
    headers
        .entry(ACCESS_CONTROL_EXPOSE_HEADERS)
        .or_insert(HeaderValue::from_static("*"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: Method, headers: &[(&str, &str)]) -> Request<()> {
        let mut builder = Request::builder().method(method).uri("/v1/messages");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn preflight_allows_what_it_is_asked_for() {
        let req = request(
            Method::OPTIONS,
            &[
                ("origin", "https://example.com"),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "authorization,content-type",
                ),
            ],
        );
        let resp = preflight(&req).expect("a preflight is answered");
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(resp.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert_eq!(resp.headers()[ACCESS_CONTROL_ALLOW_METHODS], "POST");
        // Echoed, not `*`: the wildcard would not cover `authorization`.
        assert_eq!(
            resp.headers()[ACCESS_CONTROL_ALLOW_HEADERS],
            "authorization,content-type"
        );
    }

    #[test]
    fn only_a_preflight_is_answered() {
        // An OPTIONS that requests no method is an ordinary request.
        let bare = request(Method::OPTIONS, &[("origin", "http://localhost:3000")]);
        assert!(preflight(&bare).is_none());
        let post = request(Method::POST, &[("access-control-request-method", "POST")]);
        assert!(preflight(&post).is_none());
    }

    #[test]
    fn allow_keeps_a_header_the_reply_already_carries() {
        let served = allow(Response::new(()));
        assert_eq!(served.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert_eq!(served.headers()[ACCESS_CONTROL_EXPOSE_HEADERS], "*");

        let mapped = allow(
            Response::builder()
                .header(ACCESS_CONTROL_ALLOW_ORIGIN, "https://example.com")
                .body(())
                .unwrap(),
        );
        let origins: Vec<_> = mapped
            .headers()
            .get_all(ACCESS_CONTROL_ALLOW_ORIGIN)
            .iter()
            .collect();
        assert_eq!(origins, ["https://example.com"]);
    }
}
