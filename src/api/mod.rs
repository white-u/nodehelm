//! The panel and public-status HTTP surface. This file holds what every route
//! shares: the admin guard, the same-origin write check and error responses;
//! the routes sit with their subject in the files below.

mod metrics;
mod nodes;
mod system;
#[cfg(test)]
mod tests;
mod themes;

pub use self::{metrics::*, nodes::*, system::*, themes::*};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::agent_ws::Agent;
use crate::auth::{
    authed, client_ip, current_session, hash_password, issued_at, random_token, session_cookie,
    session_expiry, sha256, with_cookies,
};
use crate::db::{self, Db, Node, NodePatch, PingTask, Traffic, TrafficPatch};
use crate::{agent_ws, App, Shared};

/// Present only on requests carrying a valid session. Handlers taking it cannot
/// be reached unauthenticated, so the check cannot be omitted.
pub struct Admin;

impl FromRequestParts<Shared> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        if authed(app, &parts.headers) {
            Ok(Admin)
        } else {
            Err(answer(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录"))
        }
    }
}

/// Whether a page other than the panel sent this request. `SameSite=Lax` holds
/// a cookie back from other *sites*, but a sibling subdomain of the hub's
/// registrable domain is the same site: its script may send a request without a
/// body (`no-cors`, no preflight) and the cookie goes along, which suffices for
/// a token reset or a vacuum. Browsers name where a request came from in
/// `Sec-Fetch-Site`; only `same-origin` is the panel itself. The header is
/// absent from a client that is not a browser, from a browser too old to send
/// it, and from every browser on a plain-http address other than loopback; the
/// last two are not told apart from the panel.
fn from_other_origin(headers: &HeaderMap) -> bool {
    headers.get("sec-fetch-site").is_some_and(|site| site != "same-origin")
}

fn cross_site_write(method: &Method, headers: &HeaderMap) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) && from_other_origin(headers)
}

/// Refuses writes from another origin before any handler sees them, so a route
/// added later is covered without remembering to ask.
pub async fn same_origin_writes(request: Request, next: Next) -> Response {
    if cross_site_write(request.method(), request.headers()) {
        return answer(StatusCode::FORBIDDEN, "请求来自其他网站，已拒绝");
    }
    next.run(request).await
}

/// Marks a response whose text was written for the reader; see [`plain_errors`].
#[derive(Clone)]
struct Written;

/// An error response carrying `text` as written. Every error the hub composes
/// is built here.
pub(crate) fn answer(status: StatusCode, text: impl Into<String>) -> Response {
    let mut response = (status, text.into()).into_response();
    response.extensions_mut().insert(Written);
    response
}

/// What a failure this hub cannot explain to the reader says instead.
pub(crate) const INTERNAL: &str = "hub 内部出错，详细原因见 hub 日志";

/// Answers an error: 400 with the outermost [`crate::Shown`] message in its
/// chain, or, without one, 500 with [`INTERNAL`]. The chain is logged in full
/// either way, the underlying cause of a shown message included.
pub(crate) fn fail(e: impl Into<anyhow::Error>) -> Response {
    let e = e.into();
    match e.downcast_ref::<crate::Shown>() {
        Some(shown) => {
            info!("request refused: {e:#}");
            answer(StatusCode::BAD_REQUEST, shown.0.clone())
        }
        None => {
            warn!("request failed: {e:#}");
            answer(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL)
        }
    }
}

fn bad(message: &str) -> Response {
    answer(StatusCode::BAD_REQUEST, message)
}

fn no_such_node() -> Response {
    answer(StatusCode::NOT_FOUND, "节点不存在，可能已被删除")
}

/// The last step of every response. An error the hub composed passes as it is;
/// any other -- an extractor's rejection, the body limit's 413, a bare status --
/// would carry axum's English wording or nothing, and is given a fixed text for
/// its status instead.
pub async fn plain_errors(response: Response) -> Response {
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error())
        || response.extensions().get::<Written>().is_some()
    {
        return response;
    }
    let text = match status {
        StatusCode::UNAUTHORIZED => "登录已失效，请重新登录",
        StatusCode::NOT_FOUND => "请求的内容不存在",
        StatusCode::PAYLOAD_TOO_LARGE => "提交的内容过大",
        s if s.is_server_error() => INTERNAL,
        _ => "请求格式不对",
    };
    // Only the body is replaced: a 405 keeps its `Allow`, for one.
    let (mut parts, _) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/plain; charset=utf-8"));
    Response::from_parts(parts, text.into())
}
