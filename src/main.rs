use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde::Serialize;
use std::{
    env,
    fs,
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tracing::{info, warn};

/// JSON body returned for every single request.
///
/// Example:
/// ```json
/// {
///   "error": {
///     "code": "not authorised",
///     "ip": "127.0.0.1"
///   }
/// }
/// ```
#[derive(Serialize)]
struct NotAuthorised {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    ip: String,
}

/// Shared application state (the access log file handle).
#[derive(Clone)]
struct AppState {
    access_log: Arc<Mutex<fs::File>>,
}

/// Catch-all handler: every path + every method returns 401 "not authorised".
/// The client IP is logged and also embedded in the JSON error body.
async fn deny_all(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    method: Method,
    _body: Bytes,
) -> Response {
    let ip = extract_ip(&addr.ip(), &headers);
    log_access(&state, &ip, &method, &uri);

    (
        StatusCode::UNAUTHORIZED,
        Json(NotAuthorised {
            error: ErrorBody {
                code: "not authorised",
                ip,
            },
        }),
    )
        .into_response()
}

/// Build the router. We register no explicit routes, so the fallback handler
/// catches *every* request.
fn app() -> Router<AppState> {
    Router::new().fallback(deny_all)
}

/// Determine the real client IP.
/// Prefers proxy headers (`X-Forwarded-For`, `X-Real-IP`) when present so the
/// address seen behind nginx/caddy/cloudflare is the actual client, falling
/// back to the TCP peer address.
fn extract_ip(socket_ip: &IpAddr, headers: &HeaderMap) -> String {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = headers.get(name) {
            if let Ok(value) = value.to_str() {
                let candidate = value.split(',').next().unwrap_or("").trim();
                if !candidate.is_empty() {
                    return candidate.to_string();
                }
            }
        }
    }
    socket_ip.to_string()
}

/// Append a line to the access log file and emit a tracing log.
fn log_access(state: &AppState, ip: &str, method: &Method, uri: &Uri) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z");
    let path = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let line = format!("{} ip={} method={} path={} status=401 error=\"not authorised\"\n", ts, ip, method, path);

    if let Ok(mut file) = state.access_log.lock() {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }

    info!(ip = %ip, method = %method, path = %uri.path(), "401 not authorised");
}

async fn serve_http(state: AppState, addr: SocketAddr) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind HTTP listener");
    let router = app().with_state(state);
    warn!("HTTP listening on http://{}", addr);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("HTTP server failed");
}

async fn serve_tls(state: AppState, addr: SocketAddr, cert_path: PathBuf, key_path: PathBuf) {
    ensure_certs(&cert_path, &key_path);

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .expect("failed to load TLS certificate/key");

    let router = app().with_state(state);
    warn!("HTTPS listening on https://{}", addr);
    axum_server::bind_rustls(addr, tls_config)
        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("HTTPS server failed");
}

/// Load the PEM cert/key from disk. If either is missing, generate a fresh
/// self-signed certificate (for local dev) and write it to the configured paths.
fn ensure_certs(cert_path: &Path, key_path: &Path) {
    if cert_path.exists() && key_path.exists() {
        return;
    }

    warn!(
        "TLS certificate/key not found, generating a self-signed cert (dev only) at {} and {}",
        cert_path.display(),
        key_path.display()
    );
    if let Some(parent) = cert_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Some(parent) = key_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let certified_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("failed to generate self-signed certificate");
    fs::write(cert_path, certified_key.cert.pem()).expect("failed to write certificate");
    fs::write(key_path, certified_key.key_pair.serialize_pem()).expect("failed to write private key");
}

fn open_log(path: &str) -> fs::File {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap_or_else(|e| panic!("failed to open access log {}: {}", path, e))
}

fn env_bool(name: &str) -> bool {
    env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let use_tls = env_bool("USE_TLS");
    // Cloudflare proxies origins on 80/443, but 80, 8080, 443 and 8443 are already
    // taken on this machine, so default to 2095 (HTTP) / 2096 (HTTPS) instead — they
    // are Cloudflare-proxyable and sit above 1024 (no root needed).
    let default_port = if use_tls { 2096 } else { 2095 };
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(default_port);
    let access_log_path = env::var("ACCESS_LOG").unwrap_or_else(|_| "logs/access.log".into());

    let state = AppState {
        access_log: Arc::new(Mutex::new(open_log(&access_log_path))),
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    if use_tls {
        let cert_path =
            PathBuf::from(env::var("TLS_CERT").unwrap_or_else(|_| "certs/server.crt".into()));
        let key_path =
            PathBuf::from(env::var("TLS_KEY").unwrap_or_else(|_| "certs/server.key".into()));
        serve_tls(state, addr, cert_path, key_path).await;
    } else {
        serve_http(state, addr).await;
    }
}
