//! Axum server: router, handlers, and application state.

use crate::auth::TokenManager;
use crate::error::Error;
use crate::llm;
use crate::proxy::ProxyClient;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, Request};
use axum::response::Response;
use axum::routing::{any, get};
use std::sync::Arc;
use tower_http::trace::MakeSpan;

#[derive(Clone)]
pub struct AppState {
    pub(crate) proxy: Arc<ProxyClient>,
}

impl AppState {
    pub async fn new() -> Result<Self, Error> {
        let token = crate::config::load_github_token()?;
        let manager = Arc::new(TokenManager::new(token).await?);
        let proxy = Arc::new(ProxyClient::new(manager)?);
        Ok(Self { proxy })
    }
}

/// Custom span factory that adds empty `initiator` and `upstream` fields to be
/// filled in later when the values are known.
#[derive(Clone)]
struct CopilotMakeSpan;

impl<B> MakeSpan<B> for CopilotMakeSpan {
    fn make_span(&mut self, request: &Request<B>) -> tracing::Span {
        tracing::info_span!(
            "request",
            method = %request.method(),
            uri = %request.uri(),
            initiator = tracing::field::Empty,
            upstream = tracing::field::Empty,
        )
    }
}

/// Record the resolved initiator and upstream path into the current request
/// span so they appear in the TraceLayer's response log line.
pub fn record_upstream(initiator: &str, path: &str) {
    let span = tracing::Span::current();
    span.record("initiator", initiator);
    span.record("upstream", path);
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/usage", get(usage_handler))
        .route("/v1/{*path}", any(proxy_handler))
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(CopilotMakeSpan))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            10 * 1024 * 1024,
        ))
        .with_state(state)
}

async fn usage_handler(State(state): State<AppState>) -> Result<Response, Error> {
    let resp = state.proxy.fetch_usage().await?;
    crate::proxy::forward_response(resp).await
}

async fn proxy_handler(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if matches!(path.as_str(), "messages" | "messages/count_tokens") {
        return llm::handle_native_claude_passthrough(
            &state,
            method,
            &path,
            uri.query(),
            &headers,
            body,
            true,
        )
        .await;
    }

    llm::handle_openai_passthrough(&state, method, &path, uri.query(), &headers, body).await
}
