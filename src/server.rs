//! Axum server: router, handlers, and application state.

use crate::auth::TokenManager;
use crate::client_auth;
use crate::error::Error;
use crate::llm;
use crate::model_policy::ModelPolicy;
use crate::policy_config::PolicyConfig;
use crate::proxy::ProxyClient;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use std::sync::Arc;
use tower_http::trace::MakeSpan;

#[derive(Clone)]
pub struct AppState {
    pub(crate) proxy: Arc<ProxyClient>,
    pub(crate) policy: Arc<ModelPolicy>,
    api_key: Option<Arc<str>>,
    super_password: Option<Arc<str>>,
}

impl AppState {
    pub async fn new() -> Result<Self, Error> {
        Self::new_with_api_key(None).await
    }

    pub async fn new_with_api_key(api_key: Option<String>) -> Result<Self, Error> {
        Self::new_with_policy_config(api_key, PolicyConfig::load(None)?).await
    }

    pub async fn new_with_policy_config(
        api_key: Option<String>,
        configuration: PolicyConfig,
    ) -> Result<Self, Error> {
        let configuration = configuration.validate()?;
        let api_key = crate::config::resolve_proxy_api_key(api_key)?.map(Arc::from);
        let state_path = configuration
            .is_configured()
            .then(|| configuration.state_path());
        let policy = ModelPolicy::new(&configuration.rules, state_path)?;
        let super_password = configuration.super_password.map(Arc::from);
        let token = crate::config::load_github_token()?;
        let manager = Arc::new(TokenManager::new(token).await?);
        let proxy = Arc::new(ProxyClient::new(manager)?);
        Ok(Self {
            proxy,
            policy,
            api_key,
            super_password,
        })
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
    let admin = admin_router(AdminState {
        policy: state.policy.clone(),
        super_password: state.super_password.clone(),
    });
    Router::new()
        .route("/v1/usage", get(usage_handler))
        .route("/v1/{*path}", any(proxy_handler))
        .route_layer(middleware::from_fn_with_state(
            state.api_key.clone(),
            client_auth::authenticate,
        ))
        .with_state(state)
        .merge(admin)
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(CopilotMakeSpan))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            10 * 1024 * 1024,
        ))
}

#[derive(Clone)]
struct AdminState {
    policy: Arc<ModelPolicy>,
    super_password: Option<Arc<str>>,
}

fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/v1/admin/limits", get(limits_handler))
        .route("/v1/admin/limits/reset", post(reset_limits_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_admin,
        ))
        .with_state(state)
}

async fn authenticate_admin(
    State(state): State<AdminState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(password) = state.super_password else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !client_auth::is_authorized(request.headers(), &password) {
        let mut response =
            Error::Auth("Missing or invalid super password".to_string()).into_response();
        response.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    next.run(request).await
}

async fn limits_handler(State(state): State<AdminState>) -> Result<Json<serde_json::Value>, Error> {
    Ok(Json(state.policy.snapshot()?))
}

async fn reset_limits_handler(
    State(state): State<AdminState>,
) -> Result<Json<serde_json::Value>, Error> {
    state.policy.reset_all()?;
    Ok(Json(
        serde_json::json!({ "reset": true, "limits": state.policy.snapshot()? }),
    ))
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
        )
        .await;
    }

    llm::handle_openai_passthrough(&state, method, &path, uri.query(), &headers, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::header;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct Upstream {
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<Value>>>,
    }

    async fn upstream_handler(
        State(state): State<Upstream>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        state.hits.fetch_add(1, Ordering::SeqCst);
        assert_eq!(headers[header::AUTHORIZATION], "Bearer test-upstream-token");
        let value: Value = serde_json::from_slice(&body).unwrap();
        state.requests.lock().unwrap().push(value.clone());
        if value["stream"] == true {
            return (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5},\"choices\":[]}\n\ndata: [DONE]\n\n",
            ).into_response();
        }
        let usage = if uri.path() == "/responses" {
            json!({"input_tokens":2,"output_tokens":3,"total_tokens":5})
        } else if uri.path().starts_with("/v1/messages") {
            json!({"input_tokens":2,"output_tokens":3})
        } else {
            json!({"prompt_tokens":2,"completion_tokens":3,"total_tokens":5})
        };
        Json(json!({"model": value["model"], "usage": usage})).into_response()
    }

    struct Fixture {
        router: Router,
        policy: Arc<ModelPolicy>,
        upstream: Upstream,
        task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new(rules: &str, password: Option<&str>) -> Self {
            let upstream = Upstream::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api_base = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .fallback(upstream_handler)
                .with_state(upstream.clone());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let policy = ModelPolicy::new(rules, None).unwrap();
            let state = AppState {
                proxy: Arc::new(ProxyClient::for_test(api_base)),
                policy: policy.clone(),
                api_key: Some(Arc::from("client-key")),
                super_password: password.map(Arc::from),
            };
            Self {
                router: create_router(state),
                policy,
                upstream,
                task,
            }
        }

        async fn request(
            &self,
            method: Method,
            path: &str,
            key: Option<&str>,
            body: Value,
        ) -> Response {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(key) = key {
                request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
            }
            self.router
                .clone()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn enforces_model_rules_on_openai_native_and_other_model_json_routes() {
        let fixture = Fixture::new("-all,+gpt-6-astra", Some("admin-secret")).await;
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/embeddings",
            "/v1/custom",
        ] {
            let response = fixture
                .request(
                    Method::POST,
                    path,
                    Some("client-key"),
                    json!({"model":"denied"}),
                )
                .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            assert_eq!(
                json_body(response).await["error"]["code"],
                "model_not_allowed"
            );
        }
        let response = fixture
            .request(
                Method::POST,
                "/v1/messages",
                Some("client-key"),
                json!({"model":"claude-sonnet-4.6","messages":[]}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(json_body(response).await["type"], "error");
        assert_eq!(fixture.upstream.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shares_model_budgets_across_apis_and_reset_requires_only_super_password() {
        let fixture =
            Fixture::new("5h:gpt-6-astra:10,7d:gpt-6-astra:10", Some("admin-secret")).await;
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let response = fixture
                .request(
                    Method::POST,
                    path,
                    Some("client-key"),
                    json!({"model":"gpt-6-astra"}),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            json_body(response).await;
        }
        let exhausted = fixture
            .request(
                Method::POST,
                "/v1/chat/completions",
                Some("client-key"),
                json!({"model":"gpt-6-astra"}),
            )
            .await;
        assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(exhausted.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(
            json_body(exhausted).await["error"]["code"],
            "model_token_limit_exceeded"
        );
        for key in [None, Some("client-key"), Some("wrong-admin")] {
            let response = fixture
                .request(Method::POST, "/v1/admin/limits/reset", key, json!({}))
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let status = fixture
            .request(
                Method::GET,
                "/v1/admin/limits",
                Some("admin-secret"),
                json!({}),
            )
            .await;
        assert_eq!(status.status(), StatusCode::OK);
        let status = json_body(status).await;
        assert_eq!(status["models"]["gpt-6-astra"]["5h"]["used"], 10);
        assert!(!status.to_string().contains("admin-secret"));
        let reset = fixture
            .request(
                Method::POST,
                "/v1/admin/limits/reset",
                Some("admin-secret"),
                json!({}),
            )
            .await;
        assert_eq!(reset.status(), StatusCode::OK);
        assert_eq!(json_body(reset).await["reset"], true);
        let response = fixture
            .request(
                Method::POST,
                "/v1/responses",
                Some("client-key"),
                json!({"model":"gpt-6-astra"}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await;
        assert_eq!(
            fixture.policy.snapshot().unwrap()["models"]["gpt-6-astra"]["7d"]["used"],
            5
        );
    }

    #[tokio::test]
    async fn admin_is_disabled_without_password_even_when_client_auth_is_enabled() {
        let fixture = Fixture::new("", None).await;
        for (method, path) in [
            (Method::GET, "/v1/admin/limits"),
            (Method::POST, "/v1/admin/limits/reset"),
        ] {
            let response = fixture
                .request(method, path, Some("client-key"), json!({}))
                .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        assert_eq!(fixture.upstream.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn limited_chat_stream_requests_usage_and_reports_overage_before_done() {
        let fixture = Fixture::new("5h:gpt-6-astra:4", None).await;
        let response = fixture.request(Method::POST, "/v1/chat/completions", Some("client-key"), json!({"model":"gpt-6-astra","stream":true,"stream_options":{"include_usage":false,"custom":1}})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let stream = String::from_utf8_lossy(&body);
        assert!(stream.contains("model_token_limit_exceeded"), "{stream}");
        assert!(!stream.contains("[DONE]"), "{stream}");
        let requests = fixture.upstream.requests.lock().unwrap();
        assert_eq!(requests[0]["stream_options"]["include_usage"], true);
        assert_eq!(requests[0]["stream_options"]["custom"], 1);
    }

    #[tokio::test]
    async fn native_count_tokens_checks_access_without_consuming_budget() {
        let fixture = Fixture::new("5h:claude-sonnet-4.6:5", None).await;
        let body = json!({"model":"claude-sonnet-4.6","messages":[],"max_tokens":3});
        let response = fixture
            .request(
                Method::POST,
                "/v1/messages",
                Some("client-key"),
                body.clone(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await;
        let response = fixture
            .request(
                Method::POST,
                "/v1/messages/count_tokens",
                Some("client-key"),
                body.clone(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await;
        let response = fixture
            .request(Method::POST, "/v1/messages", Some("client-key"), body)
            .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            json_body(response).await["error"]["type"],
            "rate_limit_error"
        );
    }
}
