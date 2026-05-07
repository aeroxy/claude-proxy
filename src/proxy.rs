use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{HeaderMap, Method, Request, Response};
use reqwest::Client;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use http_body_util::{BodyExt, Full};
use rustls::ServerConfig;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
// use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::certs::{generate_leaf_cert, CaCert};
use crate::config::{MapLocalRule, ProxyConfig};
use crate::interceptors::{
    build_map_local_response, buffered_to_response, handle_dedup_request, handle_token_request,
    handle_vertex_heatup, match_map_local, save_token_cache, token_file_to_response,
    BufferedResponse, PrimaryGuard, RequestDedupState, RequestPrimaryGuard, TokenRequestState,
    STRIPPED_RESPONSE_HEADERS,
};
use tokio::sync::broadcast;

use hyper_util::rt::TokioIo;

pub const DEFAULT_PORT: u16 = 6666;
const PORT_AUTOSHIFT_RANGE: u16 = 10;

/// Try to bind a TCP listener starting at `start_port`, incrementing on
/// `AddrInUse` up to `start_port + PORT_AUTOSHIFT_RANGE - 1`.
/// Returns the bound std listener and the port it landed on.
pub fn bind_listener(start_port: u16) -> anyhow::Result<(std::net::TcpListener, u16)> {
    let mut last_err: Option<std::io::Error> = None;
    for port in start_port..start_port.saturating_add(PORT_AUTOSHIFT_RANGE) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match std::net::TcpListener::bind(addr) {
            Ok(l) => {
                l.set_nonblocking(true)?;
                return Ok((l, port));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow::anyhow!(
        "no free port in {}..{}: {}",
        start_port,
        start_port.saturating_add(PORT_AUTOSHIFT_RANGE),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

pub async fn run_proxy(ca: CaCert, config: ProxyConfig, start_port: u16) -> anyhow::Result<()> {
    let (std_listener, _port) = bind_listener(start_port)?;
    run_proxy_with_listener(std_listener, ca, config).await
}

pub async fn run_proxy_with_listener(
    std_listener: std::net::TcpListener,
    ca: CaCert,
    config: ProxyConfig,
) -> anyhow::Result<()> {
    let local_addr = std_listener.local_addr()?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    info!("Proxy listening on http://{}", local_addr);

    let ca = Arc::new(ca);
    let mut reqwest_builder = Client::builder()
        .danger_accept_invalid_certs(true); // Always accept invalid certs as requested

    if let Some(proxy_url) = &config.upstream_proxy {
        info!("Using upstream proxy: {}", proxy_url);
        let proxy = reqwest::Proxy::all(proxy_url)?;
        reqwest_builder = reqwest_builder.proxy(proxy);
    }
    let client = Arc::new(reqwest_builder.build()?);
    let map_local = Arc::new(config.map_local.clone());
    if !map_local.is_empty() {
        info!("Loaded {} Map Local rule(s)", map_local.len());
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let ca = Arc::clone(&ca);
        let client = Arc::clone(&client);
        let map_local = Arc::clone(&map_local);

        tokio::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| {
                        handle_request(
                            req,
                            Arc::clone(&ca),
                            Arc::clone(&client),
                            Arc::clone(&map_local),
                        )
                    }),
                )
                .with_upgrades()
                .await
            {
                error!("Failed to serve connection: {}", err);
            }
        });
    }
}

async fn handle_request(
    mut req: Request<Incoming>,
    ca: Arc<CaCert>,
    client: Arc<Client>,
    map_local: Arc<Vec<MapLocalRule>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() == Method::CONNECT {
        let host = req.uri().authority().map(|auth| auth.host().to_string()).unwrap_or_default();

        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    if let Err(e) = handle_connect(upgraded, ca, host, client, map_local).await {
                        error!("Error handling CONNECT: {}", e);
                    }
                }
                Err(e) => error!("upgrade error: {}", e),
            }
        });

        Ok(Response::new(Full::new(Bytes::new())))
    } else {
        // Plain HTTP — Map Local can match `http://` URLs here. Non-mapped
        // plain HTTP keeps returning 500 to preserve the existing contract.
        let method = req.method().clone();
        let url = if req.uri().scheme().is_some() {
            req.uri().to_string()
        } else {
            let host = req
                .headers()
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            format!("http://{}{}", host, pq)
        };

        if let Some(rule) = match_map_local(&map_local, &method, &url) {
            info!("Map Local hit (plain HTTP): {} {}", method, url);
            return Ok(build_map_local_response(rule).await);
        }

        Ok(Response::builder()
            .status(500)
            .body(Full::new(Bytes::from("Only CONNECT supported")))
            .unwrap())
    }
}

async fn handle_connect(
    upgraded: Upgraded,
    ca: Arc<CaCert>,
    host: String,
    client: Arc<Client>,
    map_local: Arc<Vec<MapLocalRule>>,
) -> anyhow::Result<()> {
    let (cert, key) = generate_leaf_cert(&ca, &host)?;

    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert, key)?;

    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));
    let tls_stream = tls_acceptor.accept(TokioIo::new(upgraded)).await?;

    if let Err(err) = http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            TokioIo::new(tls_stream),
            service_fn(move |req| {
                handle_intercepted_request(
                    req,
                    host.clone(),
                    Arc::clone(&client),
                    Arc::clone(&map_local),
                )
            }),
        )
        .await
    {
        error!("Failed to serve intercepted connection: {}", err);
    }

    Ok(())
}

async fn handle_intercepted_request(
    req: Request<Incoming>,
    host: String,
    client: Arc<Client>,
    map_local: Arc<Vec<MapLocalRule>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let (parts, incoming_body) = req.into_parts();
    let body_bytes = incoming_body.collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);
    let path = parts.uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    let url = format!("https://{}{}", host, path);
    info!("Intercepted: {} {}", parts.method, url);

    if let Some(rule) = match_map_local(&map_local, &parts.method, &url) {
        let source = if rule.body.is_some() {
            "<inline>".to_string()
        } else if let Some(p) = &rule.file {
            p.display().to_string()
        } else {
            "<empty>".to_string()
        };
        info!("Map Local hit: {} {} -> {}", parts.method, url, source);
        return Ok(build_map_local_response(rule).await);
    }

    let mut primary_guard: Option<PrimaryGuard> = None;
    if host == "oauth2.googleapis.com" && path.starts_with("/token") {
        match handle_token_request(&body_str).await {
            TokenRequestState::Cached(resp) => return Ok(resp),
            TokenRequestState::Waiting(mut rx) => {
                info!("Waiting on primary in-flight token request...");
                match rx.recv().await {
                    Ok(Some(token_data)) => {
                        info!("Received token from primary in-flight request.");
                        return Ok(token_file_to_response(&token_data));
                    }
                    Ok(None) => {
                        info!("Primary explicitly returned None (failed). Fetching natively.");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("Primary channel closed without resolution (likely cancelled). Fetching natively.");
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Broadcast lagged by {}, missed primary's resolution. Fetching natively.", n);
                    }
                }
            }
            TokenRequestState::Primary(guard) => {
                info!("We are the primary fetcher, making the upstream request...");
                primary_guard = Some(guard);
            }
        }
    }

    if host == "aiplatform.googleapis.com" && path.contains(":rawPredict") {
        let parts_path = path.split('/').collect::<Vec<_>>();
        if let Some(model_part) = parts_path.iter().find(|p| p.starts_with("claude-")) {
            if let Some(heatup_resp) = handle_vertex_heatup(&body_str, model_part) {
                return Ok(heatup_resp);
            }
        }
    }

    let mut request_dedup_guard: Option<RequestPrimaryGuard> = None;
    {
        let dedup_key = format!("{} {}\n{}", parts.method, url, body_str);
        match handle_dedup_request(&dedup_key).await {
            RequestDedupState::Waiting(mut rx) => {
                info!("Waiting on primary in-flight request for {}...", url);
                match rx.recv().await {
                    Ok(Some(buf)) => {
                        info!("Received response from primary in-flight request for {}.", url);
                        return Ok(buffered_to_response(&buf));
                    }
                    Ok(None) => {
                        info!("Primary returned None (failed/non-2xx). Fetching natively.");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("Primary channel closed without resolution (likely cancelled). Fetching natively.");
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Broadcast lagged by {}, missed primary's resolution. Fetching natively.", n);
                    }
                }
            }
            RequestDedupState::Primary(guard) => {
                info!("We are the primary fetcher for {}.", url);
                request_dedup_guard = Some(guard);
            }
        }
    }

    let mut req_builder = client.request(parts.method.clone(), &url);
    for (k, v) in parts.headers.iter() {
        let key_str = k.as_str().to_lowercase();
        if key_str != "host" && key_str != "accept-encoding" && key_str != "content-length" {
            req_builder = req_builder.header(k.clone(), v.clone());
        }
    }
    
    req_builder = req_builder.body(reqwest::Body::from(body_bytes.clone()));

    tracing::debug!("Sending upstream request to {}", url);
    let send_result = req_builder.send().await;
    tracing::debug!("Upstream send() returned for {}", url);

    match send_result {
        Ok(resp) => {
            let status = resp.status();
            info!("Upstream response status for {}: {}", url, status);
            let upstream_headers = resp.headers().clone();
            let mut builder = Response::builder().status(status);

            for (k, v) in upstream_headers.iter() {
                builder = builder.header(k.clone(), v.clone());
            }

            tracing::debug!("Reading upstream response body for {}", url);
            let resp_bytes = resp.bytes().await.unwrap_or_default();
            tracing::debug!("Got {} bytes from {}", resp_bytes.len(), url);

            if let Some(guard) = request_dedup_guard.take() {
                if status.is_success() {
                    let mut filtered = HeaderMap::new();
                    for (k, v) in upstream_headers.iter() {
                        if !STRIPPED_RESPONSE_HEADERS.contains(&k.as_str().to_lowercase().as_str()) {
                            filtered.insert(k.clone(), v.clone());
                        }
                    }
                    let buf = Arc::new(BufferedResponse {
                        status: status.as_u16(),
                        headers: filtered,
                        body: resp_bytes.clone(),
                    });
                    guard.resolve(Some(buf)).await;
                } else {
                    guard.resolve(None).await;
                }
            }

            if let Some(guard) = primary_guard {
                if status.is_success() {
                    match serde_json::from_slice::<serde_json::Value>(&resp_bytes) {
                        Ok(json) => {
                            let token_file = save_token_cache(&body_str, &json).await;
                            guard.resolve(token_file).await;
                        }
                        Err(e) => {
                            warn!("Failed to parse Google OAuth JSON response: {} (body: {:?})", e, String::from_utf8_lossy(&resp_bytes));
                            guard.resolve(None).await;
                        }
                    }
                } else {
                    warn!(
                        "Google OAuth upstream returned status {}. Body: {}",
                        status,
                        String::from_utf8_lossy(&resp_bytes)
                    );

                    let is_invalid_grant = if status.as_u16() == 400 {
                        serde_json::from_slice::<serde_json::Value>(&resp_bytes)
                            .ok()
                            .and_then(|json| json.get("error")?.as_str().map(String::from))
                            .map(|e| e == "invalid_grant")
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    if is_invalid_grant {
                        warn!("Detected invalid_grant — initiating automatic re-authentication");
                        match crate::reauth::handle_invalid_grant().await {
                            Some(reauth_result) => {
                                let token_file =
                                    save_token_cache(&body_str, &reauth_result.token_response_json)
                                        .await;
                                if let Some(ref tf) = token_file {
                                    info!("Re-auth succeeded. Returning fresh token to client.");
                                    let response = token_file_to_response(tf);
                                    guard.resolve(token_file).await;
                                    return Ok(response);
                                } else {
                                    warn!("Re-auth returned tokens but couldn't cache them. Returning original error.");
                                    guard.resolve(None).await;
                                }
                            }
                            None => {
                                warn!("Re-auth failed or timed out. Returning original error to client.");
                                guard.resolve(None).await;
                            }
                        }
                    } else {
                        guard.resolve(None).await;
                    }
                }
            }

            Ok(builder.body(Full::new(resp_bytes)).unwrap())
        }
        Err(e) => {
            tracing::error!("Upstream error for {}: {}", url, e);
            if let Some(guard) = primary_guard {
                guard.resolve(None).await;
            }
            if let Some(guard) = request_dedup_guard.take() {
                guard.resolve(None).await;
            }
            Ok(Response::builder()
                .status(502)
                .body(Full::new(Bytes::from(e.to_string())))
                .unwrap())
        }
    }
}
