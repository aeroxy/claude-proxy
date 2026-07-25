use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{HeaderMap, Method, Request, Response};
use reqwest::Client;
use rustls::ServerConfig;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};
// use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Unified response body for the proxy. Most responses are fully buffered
/// (`Full<Bytes>`), but the Gemini `:streamGenerateContent` path returns a true
/// SSE stream, so every handler returns this boxed body type.
pub type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// Build a fully-buffered `ProxyBody` from raw bytes.
pub fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// Re-box a `Response<Full<Bytes>>` (returned by the interceptor helpers) into a
/// `Response<ProxyBody>` so it can be returned from the unified handlers.
pub fn box_full(resp: Response<Full<Bytes>>) -> Response<ProxyBody> {
    resp.map(|b| b.map_err(|never| match never {}).boxed())
}

/// Snapshot a response's headers for replay to dedup waiters, dropping the
/// hop-by-hop and per-client ones.
fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (k, v) in headers.iter() {
        if !STRIPPED_RESPONSE_HEADERS.contains(&k.as_str().to_lowercase().as_str()) {
            filtered.insert(k.clone(), v.clone());
        }
    }
    filtered
}

/// Resolve a routed request's dedup promise with the response we're about to
/// return, so byte-identical concurrent duplicates are served from it instead of
/// each driving its own upstream generation.
///
/// The routed Gemini/Anthropic surfaces stream, so — unlike the buffered
/// upstream-forward path — there are usually no complete bytes to hand a waiter
/// at the time the response is built. Buffered responses (error envelopes,
/// non-stream `/v1/messages`, `count_tokens`) are collected here; streams go
/// through [`RecordingBody`], which resolves the promise at EOF.
///
/// Non-2xx resolves `None`, matching the upstream-forward path: failures are
/// never replayed, waiters retry on their own.
async fn record_for_dedup(
    resp: Response<ProxyBody>,
    guard: RequestPrimaryGuard,
) -> Response<ProxyBody> {
    let status = resp.status();
    if !status.is_success() {
        guard.resolve(None).await;
        return resp;
    }

    let headers = filter_response_headers(resp.headers());
    let (parts, body) = resp.into_parts();

    // An exact size hint means the body is already in memory, so collecting it
    // is free and keeps `Content-Length` intact — no need to thread it through
    // the recorder.
    if body.size_hint().exact().is_some() {
        match body.collect().await {
            Ok(collected) => {
                let bytes = collected.to_bytes();
                guard
                    .resolve(Some(Arc::new(BufferedResponse {
                        status: status.as_u16(),
                        headers,
                        body: bytes.clone(),
                    })))
                    .await;
                return Response::from_parts(parts, full_body(bytes));
            }
            Err(e) => {
                guard.resolve(None).await;
                return Response::builder()
                    .status(502)
                    .body(full_body(Bytes::from(e.to_string())))
                    .unwrap();
            }
        }
    }

    let recorder = RecordingBody {
        inner: body,
        guard: Some(guard),
        status: status.as_u16(),
        headers,
        recording: None,
        decided: false,
    };
    Response::from_parts(parts, recorder.boxed())
}

/// Forwards a streaming body untouched while accumulating a copy, then resolves
/// the dedup promise with the whole thing at EOF.
///
/// Recording is decided **once**, on the first poll, from
/// [`RequestPrimaryGuard::has_waiters`]: with no duplicate in the wait queue
/// nothing is allocated. That decision must not be revisited mid-stream — a
/// waiter that joined after the first frame would get a truncated SSE body, so
/// late joiners are resolved with `None` and fall through to their own request.
///
/// Deliberately does not override `is_end_stream`/`size_hint`: the defaults keep
/// hyper polling until `Ready(None)`, which is what guarantees the promise is
/// resolved. If the client disconnects first, the body is dropped with the guard
/// still held and [`RequestPrimaryGuard`]'s `Drop` evicts the in-flight entry —
/// the same RAII path the upstream-forward side relies on.
struct RecordingBody {
    inner: ProxyBody,
    guard: Option<RequestPrimaryGuard>,
    status: u16,
    headers: HeaderMap,
    /// `Some` once recording has started, `None` when we decided not to record.
    recording: Option<Vec<u8>>,
    decided: bool,
}

impl RecordingBody {
    /// Resolve the promise exactly once. `complete` is false on a stream error,
    /// where the partial body must not be replayed.
    fn finish(&mut self, complete: bool) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        let payload = match (complete, self.recording.take()) {
            (true, Some(buf)) => Some(Arc::new(BufferedResponse {
                status: self.status,
                headers: std::mem::take(&mut self.headers),
                body: Bytes::from(buf),
            })),
            _ => None,
        };
        // `resolve` is async (it takes the promise-map lock) and `poll_frame`
        // can't await, so hand it off.
        tokio::spawn(async move { guard.resolve(payload).await });
    }
}

impl HttpBody for RecordingBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();

        if !this.decided {
            this.decided = true;
            if this.guard.as_ref().is_some_and(|g| g.has_waiters()) {
                this.recording = Some(Vec::new());
            }
        }

        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(buf), Some(data)) = (this.recording.as_mut(), frame.data_ref()) {
                    buf.extend_from_slice(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(false);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

use crate::certs::{generate_leaf_cert, CaCert};
use crate::config::{MapLocalRule, ProxyConfig};
use crate::interceptors::{
    buffered_to_response, build_map_local_response, handle_dedup_request, handle_token_request,
    handle_vertex_heatup, match_map_local, save_token_cache, token_file_to_response,
    BufferedResponse, PrimaryGuard, RequestDedupState, RequestPrimaryGuard, TokenRequestState,
    STRIPPED_RESPONSE_HEADERS,
};
use tokio::sync::broadcast;

use hyper_util::rt::TokioIo;

pub const DEFAULT_PORT: u16 = 7777;

/// Bind a TCP listener on `127.0.0.1:port`. Fails fast on `AddrInUse` rather than
/// hopping to another port: clients are pointed at a fixed port (default 7777),
/// so silently binding elsewhere would leave them unable to reach the proxy while
/// it appears "up". Returns the bound std listener and the (always-requested) port.
pub fn bind_listener(port: u16) -> anyhow::Result<(std::net::TcpListener, u16)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = std::net::TcpListener::bind(addr).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!(
                "127.0.0.1:{port} is already in use — another claude-proxy or process holds it. \
                 Stop it (`claude-proxy stop`) or choose another port (`--port <n>`)."
            )
        } else {
            anyhow::anyhow!("failed to bind 127.0.0.1:{port}: {e}")
        }
    })?;
    listener.set_nonblocking(true)?;
    Ok((listener, port))
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
    let mut reqwest_builder = Client::builder().danger_accept_invalid_certs(true); // Always accept invalid certs as requested

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

    let compress = Arc::new(config.compress.clone());
    if !compress.providers.is_empty() {
        info!(
            "Compression ready ({} provider(s): {})",
            compress.providers.len(),
            compress
                .providers
                .keys()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let openai = Arc::new(config.openai.clone());
    if !openai.is_empty() {
        info!(
            "OpenAI aggregator ready ({} provider(s): {})",
            openai.len(),
            openai
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let gemini = Arc::new(crate::gemini::GeminiState::new(
        config
            .settings
            .auth_dirs
            .clone()
            .unwrap_or_else(crate::gemini::creds::default_auth_dirs),
        config.settings.models_file.clone(),
        config.anthropic_model_map.clone(),
    ));
    info!("Gemini providers ready (auth dirs: {:?})", gemini.auth_dirs);
    if !gemini.anthropic_model_map.is_empty() {
        info!(
            "Anthropic model map ready ({} entr{}: {})",
            gemini.anthropic_model_map.len(),
            if gemini.anthropic_model_map.len() == 1 { "y" } else { "ies" },
            gemini
                .anthropic_model_map
                .iter()
                .map(|(from, to)| format!("{from} -> {to}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let ca = Arc::clone(&ca);
        let client = Arc::clone(&client);
        let map_local = Arc::clone(&map_local);
        let gemini = Arc::clone(&gemini);
        let openai = Arc::clone(&openai);
        let compress = Arc::clone(&compress);

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
                            Arc::clone(&gemini),
                            Arc::clone(&openai),
                            Arc::clone(&compress),
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
    gemini: Arc<crate::gemini::GeminiState>,
    openai: Arc<Vec<crate::config::OpenAIProvider>>,
    compress: Arc<crate::compress::CompressConfig>,
) -> Result<Response<ProxyBody>, hyper::Error> {
    if req.method() == Method::CONNECT {
        let host = req
            .uri()
            .authority()
            .map(|auth| auth.host().to_string())
            .unwrap_or_default();

        tokio::spawn(async move {
            match hyper::upgrade::on(&mut req).await {
                Ok(upgraded) => {
                    if let Err(e) =
                        handle_connect(upgraded, ca, host, client, map_local, gemini, compress)
                            .await
                    {
                        error!("Error handling CONNECT: {}", e);
                    }
                }
                Err(e) => error!("upgrade error: {}", e),
            }
        });

        Ok(Response::new(full_body(Bytes::new())))
    } else {
        // Plain HTTP. Map Local can match `http://` URLs here, and we serve the
        // Gemini API as a plain-HTTP origin (opencode `@ai-sdk/google` baseURL
        // pointed at us). Everything else keeps returning 500.
        let method = req.method().clone();
        let (parts, incoming_body) = req.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        let url = if parts.uri.scheme().is_some() {
            parts.uri.to_string()
        } else {
            let host = parts
                .headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            format!("http://{}{}", host, path)
        };

        if let Some(rule) = match_map_local(&map_local, &method, &url) {
            info!("Map Local hit (plain HTTP): {} {}", method, url);
            return Ok(box_full(build_map_local_response(rule).await));
        }

        if crate::gemini::is_gemini_path(&path) {
            let raw_body = incoming_body.collect().await?.to_bytes();
            let body_bytes = if compress.providers.is_empty() {
                raw_body
            } else {
                compress_gemini_body_async(raw_body, path.clone(), (*compress).clone()).await
            };
            if let Some(resp) =
                crate::gemini::try_handle(&method, &path, body_bytes, &client, &gemini).await
            {
                return Ok(resp);
            }
        } else if crate::gemini::anthropic::is_messages_path(&path) {
            let raw_body = incoming_body.collect().await?.to_bytes();
            // Same duplicate-collapsing as the MITM branch (see the comment on
            // the Anthropic gate in `handle_intercepted_request`). This branch
            // has no upstream-forward fallback, so dedup only exists here.
            let dedup_key = format!("{} {}\n{}", method, url, String::from_utf8_lossy(&raw_body));
            let mut dedup_guard: Option<RequestPrimaryGuard> = None;
            match handle_dedup_request(&dedup_key).await {
                RequestDedupState::Waiting(mut rx) => {
                    info!("Waiting on primary in-flight routed request for {}...", url);
                    match rx.recv().await {
                        Ok(Some(buf)) => {
                            info!(
                                "Received response from primary in-flight routed request for {}.",
                                url
                            );
                            return Ok(box_full(buffered_to_response(&buf)));
                        }
                        Ok(None) => {
                            info!("Primary returned None (failed/non-2xx/unrecorded). Serving natively.");
                        }
                        Err(e) => {
                            warn!("Primary did not resolve ({}). Serving natively.", e);
                        }
                    }
                }
                RequestDedupState::Primary(guard) => {
                    info!("We are the primary fetcher for routed request {}.", url);
                    dedup_guard = Some(guard);
                }
            }

            let body_bytes = if compress.providers.is_empty() {
                raw_body
            } else {
                crate::compress::maybe_apply_async(raw_body, (*compress).clone()).await
            };
            match crate::gemini::anthropic::try_handle(
                &method, &path, body_bytes, &client, &gemini,
            )
            .await
            {
                Some(resp) => {
                    return Ok(match dedup_guard {
                        Some(guard) => record_for_dedup(resp, guard).await,
                        None => resp,
                    })
                }
                None => {
                    if let Some(guard) = dedup_guard {
                        guard.resolve(None).await;
                    }
                }
            }
        } else if crate::openai::is_chat_completions_path(&path)
            || crate::gemini::openai::is_chat_completions_path(&path)
        {
            // `/v1/chat/completions` origin. Two surfaces share this path:
            //   1. Gemini providers (gemini-cli/<model>, antigravity/<model>) —
            //      translated to the Cloud Code Assist upstreams.
            //   2. The `[[openai]]` aggregator — pure passthrough to configured
            //      OpenAI-compatible backends.
            // Routing is by provider prefix on the body's `model`: a Gemini
            // prefix wins; otherwise the aggregator handles it (which itself
            // requires an `[[openai]]` provider prefix).
            let raw_body = incoming_body.collect().await?.to_bytes();
            let body_bytes = if compress.providers.is_empty() {
                raw_body
            } else {
                crate::compress::maybe_apply_async(raw_body, (*compress).clone()).await
            };
            if crate::gemini::openai::model_has_provider_prefix(&body_bytes) {
                if let Some(resp) =
                    crate::gemini::openai::try_handle(&method, &path, body_bytes, &client, &gemini)
                        .await
                {
                    return Ok(resp);
                }
            } else {
                let incoming_auth = parts.headers.get(hyper::header::AUTHORIZATION).cloned();
                if let Some(resp) = crate::openai::try_handle(
                    &method,
                    &path,
                    body_bytes,
                    &client,
                    &openai,
                    incoming_auth,
                )
                .await
                {
                    return Ok(resp);
                }
            }
        }

        warn!(
            "Unhandled plain-HTTP request — returning 500: {} {}",
            method, url
        );
        Ok(Response::builder()
            .status(500)
            .body(full_body(Bytes::from("Only CONNECT supported")))
            .unwrap())
    }
}

async fn handle_connect(
    upgraded: Upgraded,
    ca: Arc<CaCert>,
    host: String,
    client: Arc<Client>,
    map_local: Arc<Vec<MapLocalRule>>,
    gemini: Arc<crate::gemini::GeminiState>,
    compress: Arc<crate::compress::CompressConfig>,
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
                    Arc::clone(&gemini),
                    Arc::clone(&compress),
                )
            }),
        )
        .await
    {
        // hyper returns an error here mainly when the connection can't be shut
        // down gracefully — almost always because the client already closed or
        // aborted (keep-alive close, Ctrl-C, end of an SSE stream). That's
        // routine teardown, not a server fault, so keep it at debug.
        debug!("Intercepted connection closed early: {}", err);
    }

    Ok(())
}

async fn handle_intercepted_request(
    req: Request<Incoming>,
    host: String,
    client: Arc<Client>,
    map_local: Arc<Vec<MapLocalRule>>,
    gemini: Arc<crate::gemini::GeminiState>,
    compress: Arc<crate::compress::CompressConfig>,
) -> Result<Response<ProxyBody>, hyper::Error> {
    let (parts, incoming_body) = req.into_parts();
    let mut body_bytes = incoming_body.collect().await?.to_bytes();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

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
        return Ok(box_full(build_map_local_response(rule).await));
    }

    // Gemini API (opencode @ai-sdk/google) via MITM of the default Google host.
    if host == crate::gemini::GEMINI_UPSTREAM_HOST && crate::gemini::is_gemini_path(path) {
        let compressed = if compress.providers.is_empty() {
            body_bytes.clone()
        } else {
            compress_gemini_body_async(body_bytes.clone(), path.to_string(), (*compress).clone())
                .await
        };
        if let Some(resp) =
            crate::gemini::try_handle(&parts.method, path, compressed, &client, &gemini).await
        {
            return Ok(resp);
        }
    }

    // Anthropic Messages API via MITM of api.anthropic.com — gated on the body's
    // `model` being routable (a provider prefix, or an exact `[anthropic_model_map]`
    // entry) so only requests meant for us are served; everything else falls
    // through to the real Anthropic API untouched, so the normal `claude` CLI
    // keeps working.
    if host == crate::gemini::anthropic::ANTHROPIC_UPSTREAM_HOST
        && crate::gemini::anthropic::is_messages_path(path)
        && crate::gemini::anthropic::model_is_routable(&body_bytes, &gemini.anthropic_model_map)
    {
        // Dedup applies here too, not just on the upstream-forward path below:
        // this early return jumps over that block, and without it a client that
        // fires byte-identical concurrent requests (Claude Code does exactly
        // that for session-title generation) burns one provider generation per
        // duplicate. Keyed on the pre-compression body — what the client sent —
        // in the same `method url\nbody` shape the forward path uses, so a
        // routed and a forwarded request can never collide (routability is a
        // pure function of the body).
        let dedup_key = format!(
            "{} {}\n{}",
            parts.method,
            url,
            String::from_utf8_lossy(&body_bytes)
        );
        let mut dedup_guard: Option<RequestPrimaryGuard> = None;
        match handle_dedup_request(&dedup_key).await {
            RequestDedupState::Waiting(mut rx) => {
                info!("Waiting on primary in-flight routed request for {}...", url);
                match rx.recv().await {
                    Ok(Some(buf)) => {
                        info!(
                            "Received response from primary in-flight routed request for {}.",
                            url
                        );
                        return Ok(box_full(buffered_to_response(&buf)));
                    }
                    Ok(None) => {
                        info!("Primary returned None (failed/non-2xx/unrecorded). Serving natively.");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("Primary channel closed without resolution (likely cancelled). Serving natively.");
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Broadcast lagged by {}, missed primary's resolution. Serving natively.", n);
                    }
                }
            }
            RequestDedupState::Primary(guard) => {
                info!("We are the primary fetcher for routed request {}.", url);
                dedup_guard = Some(guard);
            }
        }

        let compressed = if compress.providers.is_empty() {
            body_bytes.clone()
        } else {
            crate::compress::maybe_apply_async(body_bytes.clone(), (*compress).clone()).await
        };
        match crate::gemini::anthropic::try_handle(
            &parts.method,
            path,
            compressed,
            &client,
            &gemini,
        )
        .await
        {
            Some(resp) => {
                return Ok(match dedup_guard {
                    Some(guard) => record_for_dedup(resp, guard).await,
                    None => resp,
                })
            }
            // Unreachable in practice (`try_handle` only declines paths that
            // `is_messages_path` already rejected), but the guard must be
            // resolved before falling through — the forward path below rebuilds
            // the same key and would otherwise wait on our own promise forever.
            None => {
                if let Some(guard) = dedup_guard {
                    guard.resolve(None).await;
                }
            }
        }
    }

    // Vertex AI Anthropic (e.g. streamRawPredict) — compress request body
    // using the "vertex" provider config before forwarding upstream.
    // Two-clause host match: bare domain and the production regional form
    // `{LOCATION}-aiplatform.googleapis.com` (e.g. `us-central1-aiplatform.googleapis.com`).
    if host == "aiplatform.googleapis.com" || host.ends_with("-aiplatform.googleapis.com") {
        if let Some(provider) = crate::compress::vertex_provider_from_path(path) {
            body_bytes = if compress.providers.is_empty() {
                body_bytes
            } else {
                compress_vertex_async(body_bytes, provider, (*compress).clone()).await
            };
        }
    }

    let body_str = String::from_utf8_lossy(&body_bytes);
    let mut primary_guard: Option<PrimaryGuard> = None;
    if host == "oauth2.googleapis.com" && path.starts_with("/token") {
        match handle_token_request(&body_str).await {
            TokenRequestState::Cached(resp) => return Ok(box_full(resp)),
            TokenRequestState::Waiting(mut rx) => {
                info!("Waiting on primary in-flight token request...");
                match rx.recv().await {
                    Ok(Some(token_data)) => {
                        info!("Received token from primary in-flight request.");
                        return Ok(box_full(token_file_to_response(&token_data)));
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

    if (host == "aiplatform.googleapis.com" || host.ends_with("-aiplatform.googleapis.com"))
        && path.contains(":rawPredict")
    {
        let parts_path = path.split('/').collect::<Vec<_>>();
        if let Some(model_part) = parts_path.iter().find(|p| p.starts_with("claude-")) {
            if let Some(heatup_resp) = handle_vertex_heatup(&body_str, model_part) {
                return Ok(box_full(heatup_resp));
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
                        info!(
                            "Received response from primary in-flight request for {}.",
                            url
                        );
                        return Ok(box_full(buffered_to_response(&buf)));
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
            let resp_bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    // A mid-body read failure must not be masked as an empty 2xx:
                    // that would resolve dedup waiters with an empty success buffer
                    // and hand the client a bogus empty body. Treat it exactly like
                    // the send() error path — fail both guards and return 502.
                    tracing::error!("Upstream body read error for {}: {}", url, e);
                    if let Some(guard) = primary_guard {
                        guard.resolve(None).await;
                    }
                    if let Some(guard) = request_dedup_guard.take() {
                        guard.resolve(None).await;
                    }
                    return Ok(Response::builder()
                        .status(502)
                        .body(full_body(Bytes::from(e.to_string())))
                        .unwrap());
                }
            };
            tracing::debug!("Got {} bytes from {}", resp_bytes.len(), url);

            if let Some(guard) = request_dedup_guard.take() {
                if status.is_success() {
                    let buf = Arc::new(BufferedResponse {
                        status: status.as_u16(),
                        headers: filter_response_headers(&upstream_headers),
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
                            warn!(
                                "Failed to parse Google OAuth JSON response: {} (body: {:?})",
                                e,
                                String::from_utf8_lossy(&resp_bytes)
                            );
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
                                    let response = box_full(token_file_to_response(tf));
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

            Ok(builder.body(full_body(resp_bytes)).unwrap())
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
                .body(full_body(Bytes::from(e.to_string())))
                .unwrap())
        }
    }
}

fn compress_gemini_body(
    body: Bytes,
    path: &str,
    compress: &crate::compress::CompressConfig,
) -> Bytes {
    if compress.providers.is_empty() {
        return body;
    }
    let provider = crate::compress::gemini_provider_from_path(path);
    if let Some(p) = provider {
        crate::compress::apply(body, &p, compress)
    } else {
        crate::compress::maybe_apply(body, compress)
    }
}

/// Async wrapper around [`compress_gemini_body`] — see
/// [`crate::compress::maybe_apply_async`] for rationale.
async fn compress_gemini_body_async(
    body: Bytes,
    path: String,
    compress: crate::compress::CompressConfig,
) -> Bytes {
    if compress.providers.is_empty() {
        return body;
    }
    let original_body = body.clone();
    match tokio::task::spawn_blocking(move || compress_gemini_body(body, &path, &compress)).await {
        Ok(res) => res,
        Err(_) => original_body,
    }
}

/// Async wrapper for the Vertex AI compression path — offloads the
/// CPU-bound `compress::apply` to the blocking thread pool, consistent
/// with `compress_gemini_body_async` and `compress::maybe_apply_async`.
async fn compress_vertex_async(
    body: Bytes,
    provider: String,
    compress: crate::compress::CompressConfig,
) -> Bytes {
    if compress.providers.is_empty() {
        return body;
    }
    let original_body = body.clone();
    match tokio::task::spawn_blocking(move || crate::compress::apply(body, &provider, &compress))
        .await
    {
        Ok(res) => res,
        Err(_) => original_body,
    }
}
