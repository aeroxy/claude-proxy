use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response};
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
use crate::config::ProxyConfig;
use crate::interceptors::{handle_token_request, handle_vertex_heatup, save_token_cache, PrimaryGuard, TokenRequestState, token_file_to_response};
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

    loop {
        let (stream, _) = listener.accept().await?;
        let ca = Arc::clone(&ca);
        let client = Arc::clone(&client);

        tokio::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |req| handle_request(req, Arc::clone(&ca), Arc::clone(&client))),
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
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() == Method::CONNECT {
        let host = req.uri().authority().map(|auth| auth.host().to_string()).unwrap_or_default();
        
        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    if let Err(e) = handle_connect(upgraded, ca, host, client).await {
                        error!("Error handling CONNECT: {}", e);
                    }
                }
                Err(e) => error!("upgrade error: {}", e),
            }
        });

        Ok(Response::new(Full::new(Bytes::new())))
    } else {
        // Handle non-CONNECT (e.g. plain HTTP) which shouldn't happen much for claude CLI but good to have
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
            service_fn(move |req| handle_intercepted_request(req, host.clone(), Arc::clone(&client))),
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
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let (parts, incoming_body) = req.into_parts();
    let body_bytes = incoming_body.collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);
    let path = parts.uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

    let url = format!("https://{}{}", host, path);
    info!("Intercepted: {} {}", parts.method, url);

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
            let mut builder = Response::builder().status(status);

            for (k, v) in resp.headers().iter() {
                builder = builder.header(k.clone(), v.clone());
            }

            tracing::debug!("Reading upstream response body for {}", url);
            let resp_bytes = resp.bytes().await.unwrap_or_default();
            tracing::debug!("Got {} bytes from {}", resp_bytes.len(), url);

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
                    guard.resolve(None).await;
                }
            }

            Ok(builder.body(Full::new(resp_bytes)).unwrap())
        }
        Err(e) => {
            tracing::error!("Upstream error for {}: {}", url, e);
            if let Some(guard) = primary_guard {
                guard.resolve(None).await;
            }
            Ok(Response::builder()
                .status(502)
                .body(Full::new(Bytes::from(e.to_string())))
                .unwrap())
        }
    }
}
