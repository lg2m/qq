use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use qq_core::WorkspaceAccess;
use qq_protocol::{ServerInfo, WorkspaceSummary};
use reqwest::Url;
use serde::Serialize;
use thiserror::Error;

use crate::{ServerError, generate_bearer_token};

const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_PAIRINGS: usize = 8;
const MAX_SESSIONS: usize = 16;
const COOKIE_NAME: &str = "qq_session";

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

#[derive(Debug, Clone)]
pub struct WebOptions {
    origin: Option<String>,
    workspaces: Vec<WorkspaceSummary>,
}

impl WebOptions {
    #[must_use]
    pub fn new(workspaces: Vec<WorkspaceSummary>) -> Self {
        Self {
            origin: None,
            workspaces,
        }
    }

    #[must_use]
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }
}

#[derive(Clone)]
pub(crate) struct AuthenticatedRequest {
    pub access: WorkspaceAccess,
    pub web_session: Option<String>,
    pub csrf_token: Option<String>,
}

impl AuthenticatedRequest {
    pub const fn native() -> Self {
        Self {
            access: WorkspaceAccess::all(),
            web_session: None,
            csrf_token: None,
        }
    }
}

struct BrowserSession {
    csrf_token: String,
    expires_at: Instant,
}

pub(crate) struct WebState {
    origin: String,
    workspaces: Vec<WorkspaceSummary>,
    pairings: Mutex<HashMap<String, Instant>>,
    sessions: Mutex<HashMap<String, BrowserSession>>,
}

impl WebState {
    pub fn new(
        options: WebOptions,
        local_origin: String,
    ) -> Result<(Arc<Self>, String), ServerError> {
        if options.workspaces.is_empty() {
            return Err(ServerError::InvalidWebConfiguration(
                "at least one web workspace is required".to_owned(),
            ));
        }
        let origin = normalize_origin(&options.origin.unwrap_or(local_origin))?;
        let state = Arc::new(Self {
            origin,
            workspaces: options.workspaces,
            pairings: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        });
        let url = state.issue_pairing()?;
        Ok((state, url))
    }

    pub fn issue_pairing(&self) -> Result<String, ServerError> {
        let secret = generate_bearer_token()?;
        let mut pairings = self.pairings.lock().map_err(|_| ServerError::WebState)?;
        let now = Instant::now();
        pairings.retain(|_, expires| *expires > now);
        if pairings.len() >= MAX_PAIRINGS
            && let Some(oldest) = pairings
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(secret, _)| secret.clone())
        {
            pairings.remove(&oldest);
        }
        pairings.insert(secret.clone(), now + PAIRING_TTL);
        Ok(format!(
            "{}/#pair={secret}",
            self.origin.trim_end_matches('/')
        ))
    }

    pub fn pair(&self, secret: &str) -> Result<(String, WebBootstrap), PairError> {
        let now = Instant::now();
        let accepted = self
            .pairings
            .lock()
            .map_err(|_| PairError::Unavailable)?
            .remove(secret)
            .is_some_and(|expires| expires > now);
        if !accepted {
            return Err(PairError::Invalid);
        }
        let session_id = generate_bearer_token().map_err(|_| PairError::Unavailable)?;
        let csrf_token = generate_bearer_token().map_err(|_| PairError::Unavailable)?;
        let mut sessions = self.sessions.lock().map_err(|_| PairError::Unavailable)?;
        sessions.retain(|_, session| session.expires_at > now);
        if sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| session.expires_at)
                .map(|(id, _)| id.clone())
        {
            sessions.remove(&oldest);
        }
        sessions.insert(
            session_id.clone(),
            BrowserSession {
                csrf_token: csrf_token.clone(),
                expires_at: now + SESSION_TTL,
            },
        );
        Ok((session_id, self.bootstrap(csrf_token)))
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Option<AuthenticatedRequest> {
        let token = cookie(headers, COOKIE_NAME)?;
        let now = Instant::now();
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(token)?;
        if session.expires_at <= now {
            return None;
        }
        Some(AuthenticatedRequest {
            access: WorkspaceAccess::only(self.workspaces.iter().map(|workspace| workspace.id)),
            web_session: Some(token.to_owned()),
            csrf_token: Some(session.csrf_token.clone()),
        })
    }

    pub fn authorize_browser_request(
        &self,
        method: &Method,
        headers: &HeaderMap,
        request: &AuthenticatedRequest,
    ) -> bool {
        if matches!(*method, Method::GET | Method::HEAD) {
            return true;
        }
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| origin == self.origin)
            && headers
                .get("x-qq-csrf")
                .and_then(|value| value.to_str().ok())
                .zip(request.csrf_token.as_deref())
                .is_some_and(|(actual, expected)| actual == expected)
    }

    pub fn origin_matches(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| origin == self.origin)
    }

    pub fn bootstrap_for(&self, request: &AuthenticatedRequest) -> Option<WebBootstrap> {
        Some(self.bootstrap(request.csrf_token.clone()?))
    }

    pub fn logout(&self, request: &AuthenticatedRequest) {
        if let Some(session) = &request.web_session
            && let Ok(mut sessions) = self.sessions.lock()
        {
            sessions.remove(session);
        }
    }

    pub fn permits_model_path(&self, access: &WorkspaceAccess, path: &str) -> bool {
        !access.is_restricted()
            || self
                .workspaces
                .iter()
                .any(|workspace| access.permits(workspace.id) && workspace.path == path)
    }

    fn bootstrap(&self, csrf_token: String) -> WebBootstrap {
        WebBootstrap {
            server: ServerInfo {
                protocol_version: qq_protocol::PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: std::process::id(),
            },
            csrf_token,
            workspaces: self.workspaces.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct WebBootstrap {
    server: ServerInfo,
    csrf_token: String,
    workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Error)]
pub(crate) enum PairError {
    #[error("pairing link is invalid or expired")]
    Invalid,
    #[error("pairing is unavailable")]
    Unavailable,
}

pub(crate) fn session_cookie(session_id: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={session_id}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict",
        SESSION_TTL.as_secs()
    ))
    .expect("generated cookie is a valid header")
}

pub(crate) fn expired_cookie() -> HeaderValue {
    HeaderValue::from_static("qq_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

pub(crate) fn asset_response(path: &str) -> Response {
    let requested = if path == "/" { "/index.html" } else { path };
    let asset = WEB_ASSETS
        .iter()
        .find(|(route, _, _)| *route == requested)
        .or_else(|| {
            (!path.starts_with("/v1/")).then(|| {
                WEB_ASSETS
                    .iter()
                    .find(|(route, _, _)| *route == "/index.html")
                    .expect("web build contains index.html")
            })
        });
    let Some((route, content_type, bytes)) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache = if *route == "/index.html" {
        "no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
    let mut response = Response::new(Body::from(*bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
}

fn normalize_origin(origin: &str) -> Result<String, ServerError> {
    let url = Url::parse(origin).map_err(|_| {
        ServerError::InvalidWebConfiguration("web origin must be an absolute URL".to_owned())
    })?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !local_http {
        return Err(ServerError::InvalidWebConfiguration(
            "web origin must use HTTPS unless it is localhost".to_owned(),
        ));
    }
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(ServerError::InvalidWebConfiguration(
            "web origin must contain only scheme, host, and optional port".to_owned(),
        ));
    }
    Ok(url.origin().ascii_serialization())
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|item| item.strip_prefix(name)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origins_are_canonical_and_require_secure_remote_transport() {
        assert_eq!(
            normalize_origin("https://qq.example.test/").unwrap(),
            "https://qq.example.test"
        );
        assert_eq!(
            normalize_origin("http://127.0.0.1:1234").unwrap(),
            "http://127.0.0.1:1234"
        );
        assert!(normalize_origin("http://qq.example.test").is_err());
        assert!(normalize_origin("https://qq.example.test/path").is_err());
    }
}
