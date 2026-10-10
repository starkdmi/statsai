//! Loopback API + file-watching daemon for `statsai`.

pub mod http;
mod ingest;

pub use http::{Header, HttpLimits, ListenAddr, Method, Request, Response, Server, StatusCode};
pub use ingest::ingest_sync_batch;

use anyhow::{bail, Context, Result};
use serde_json::json;
use statsai_core::SyncBatch;
use statsai_store::Store;
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_SYNC_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn lock_store(store: &Arc<Mutex<Store>>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn run(addr: &str, store: Arc<Mutex<Store>>, auth_token: &str) -> Result<()> {
    let bind_addr = resolve_loopback_addr(addr)?;
    let server = Server::http(bind_addr)
        .map_err(|err| anyhow::anyhow!("start local API on {bind_addr}: {err}"))?;

    for request in server.incoming_requests() {
        if let Err(error) = handle_request(request, &store, auth_token) {
            eprintln!("daemon: request failed: {error:#}");
        }
    }

    Ok(())
}

fn handle_request(mut request: Request, store: &Arc<Mutex<Store>>, auth_token: &str) -> Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();

    if let Err(rejection) = validate_http_request(
        &method,
        &url,
        request.headers(),
        request.body_length(),
        auth_token,
    ) {
        return respond_text(request, rejection.status, rejection.message);
    }

    if method == Method::Post && url == "/v1/sync/batches" {
        let mut body = Vec::with_capacity(
            request
                .body_length()
                .unwrap_or_default()
                .min(MAX_SYNC_BATCH_BYTES),
        );
        request
            .as_reader()
            .take((MAX_SYNC_BATCH_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .context("read sync batch request")?;
        if body.len() > MAX_SYNC_BATCH_BYTES {
            return respond_text(request, StatusCode(413), "sync batch is too large");
        }
        let batch: SyncBatch = match serde_json::from_slice(&body) {
            Ok(batch) => batch,
            Err(error) => {
                return respond_text(request, StatusCode(400), &format!("invalid batch: {error}"));
            }
        };
        let ack = {
            let s = lock_store(store);
            match ingest_sync_batch(&s, &batch) {
                Ok(ack) => ack,
                Err(error) => {
                    return respond_text(request, StatusCode(400), &error.to_string());
                }
            }
        };
        return respond_json(request, StatusCode(200), &ack);
    }

    if method != Method::Get {
        return respond_text(request, StatusCode(405), "method not allowed");
    }

    if url == "/health" {
        return respond_json(request, StatusCode(200), &health_payload());
    }

    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    if path == "/reports/cache" {
        let report_query = match cache_report_query(query, chrono::Utc::now()) {
            Ok(report_query) => report_query,
            Err(error) => return respond_text(request, StatusCode(400), &format!("{error:#}")),
        };
        let report = lock_store(store).cache_report(&report_query, &chrono::Local)?;
        return respond_json(request, StatusCode(200), &report);
    }

    let s = lock_store(store);
    let payload = match url.as_str() {
        "/status" => json!({
            "events": s.event_count()?,
            "tokens": s.token_total()?
        }),
        "/sources" => serde_json::to_value(s.list_sources()?)?,
        "/accounts" => serde_json::to_value(s.list_accounts()?)?,
        "/source-account-assignments" => {
            serde_json::to_value(s.list_source_account_assignments()?)?
        }
        "/subscriptions" => serde_json::to_value(s.list_subscriptions()?)?,
        "/reports/weekly" => json!({
            "events": s.event_count()?,
            "tokens": s.token_total()?
        }),
        _ => {
            drop(s);
            return respond_text(request, StatusCode(404), "not found");
        }
    };
    drop(s);

    respond_json(request, StatusCode(200), &payload)
}

/// Reads `from`, `to`, `provider`, `account`, `session`, and `timeline` from
/// a percent-encoded query. Per-call details are left to the CLI.
fn cache_report_query(
    query: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<statsai_store::CacheReportQuery> {
    let mut params = std::collections::BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .context("query values must be UTF-8")?;
        if params.insert(key, value.into_owned()).is_some() {
            bail!("repeated query parameter {key}");
        }
    }
    let (from, to) = (params.remove("from"), params.remove("to"));
    let mut report_query = match params.remove("all").as_deref() {
        None | Some("0" | "false") => {
            statsai_store::CacheReportQuery::for_range(from.as_deref(), to.as_deref(), now)?
        }
        Some("1" | "true") if from.is_none() && to.is_none() => {
            statsai_store::CacheReportQuery::all_time(now)
        }
        Some("1" | "true") => bail!("all cannot be combined with from or to"),
        Some(other) => bail!("all must be true or false, not {other}"),
    };
    report_query.provider = params.remove("provider");
    report_query.account = params.remove("account");
    report_query.session = params.remove("session");
    report_query.timeline = match params.remove("timeline").as_deref() {
        None | Some("0" | "false") => false,
        Some("1" | "true") => true,
        Some(other) => bail!("timeline must be true or false, not {other}"),
    };
    if let Some(key) = params.keys().next() {
        bail!("unknown query parameter {key}");
    }
    Ok(report_query)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpRejection {
    status: StatusCode,
    message: &'static str,
}

fn validate_http_request(
    method: &Method,
    url: &str,
    headers: &[Header],
    body_length: Option<usize>,
    auth_token: &str,
) -> std::result::Result<(), HttpRejection> {
    if headers.iter().any(|header| header.field.equiv("Origin")) {
        return Err(HttpRejection {
            status: StatusCode(403),
            message: "browser-originated requests are not allowed",
        });
    }

    if method == &Method::Get && url == "/health" {
        return Ok(());
    }

    let mut authorization_headers = headers
        .iter()
        .filter(|header| header.field.equiv("Authorization"));
    let supplied_token = authorization_headers
        .next()
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "));
    if authorization_headers.next().is_some()
        || !supplied_token.is_some_and(|token| constant_time_eq(token, auth_token))
    {
        return Err(HttpRejection {
            status: StatusCode(401),
            message: "missing or invalid bearer token",
        });
    }

    if method == &Method::Post && url == "/v1/sync/batches" {
        let mut content_type_headers = headers
            .iter()
            .filter(|header| header.field.equiv("Content-Type"));
        let is_json = content_type_headers.next().is_some_and(|header| {
            header
                .value
                .as_str()
                .split(';')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        });
        if content_type_headers.next().is_some() || !is_json {
            return Err(HttpRejection {
                status: StatusCode(415),
                message: "content-type must be application/json",
            });
        }
        if body_length.is_some_and(|length| length > MAX_SYNC_BATCH_BYTES) {
            return Err(HttpRejection {
                status: StatusCode(413),
                message: "sync batch is too large",
            });
        }
    }

    Ok(())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn health_payload() -> serde_json::Value {
    json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn respond_json<T: serde::Serialize>(
    request: Request,
    status: StatusCode,
    payload: &T,
) -> Result<()> {
    let body = serde_json::to_string_pretty(payload)?;
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(content_type_json());
    request.respond(response)?;
    Ok(())
}

fn respond_text(request: Request, status: StatusCode, body: &str) -> Result<()> {
    let response = Response::from_string(body).with_status_code(status);
    request.respond(response)?;
    Ok(())
}

fn content_type_json() -> Header {
    Header::from_bytes("content-type", "application/json").expect("static header is valid")
}

#[cfg(feature = "watch")]
mod watch;

#[cfg(not(feature = "watch"))]
pub fn watch_and_serve(
    _addr: &str,
    _store: Arc<Mutex<Store>>,
    _device_id: &str,
    _auth_token: &str,
    _scan_lock: Option<&std::path::Path>,
) -> Result<()> {
    anyhow::bail!(
        "daemon --watch requires the `watch` cargo feature (enable with --features watch)"
    )
}

/// Serves the loopback API and rescans sources as their files change.
///
/// `scan_lock` is the lock file other scanners of this store take, normally
/// `statsai_store::scan_lock_path(store_path)`. Each rescan holds it and only
/// that long: while another process holds it the pass waits, and the changed
/// paths stay queued until the lock is free. `None` scans without it, and so
/// does a pass that cannot open or lock the file, after one warning.
#[cfg(feature = "watch")]
pub fn watch_and_serve(
    addr: &str,
    store: Arc<Mutex<Store>>,
    device_id: &str,
    auth_token: &str,
    scan_lock: Option<&std::path::Path>,
) -> Result<()> {
    watch::watch_and_serve(addr, store, device_id, auth_token, scan_lock)
}

fn resolve_loopback_addr(addr: &str) -> Result<SocketAddr> {
    let resolved = addr.to_socket_addrs()?.collect::<Vec<_>>();
    validated_loopback_addr(&resolved)
}

fn validated_loopback_addr(resolved: &[SocketAddr]) -> Result<SocketAddr> {
    let Some(first) = resolved.first().copied() else {
        bail!("local API address did not resolve");
    };
    if resolved.iter().any(|addr| !addr.ip().is_loopback()) {
        bail!("local API address must resolve exclusively to loopback addresses");
    }
    Ok(first)
}

#[cfg(test)]
mod tests;
