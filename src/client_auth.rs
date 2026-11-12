//! Optional client bearer authentication, independent of upstream credentials.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub(crate) async fn authenticate(
    State(api_key): State<Option<Arc<str>>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(api_key) = api_key
        && !is_authorized(request.headers(), &api_key)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(serde_json::json!({
                "error": {
                    "message": "Missing or invalid API key. Use Authorization: Bearer <API key>.",
                    "type": "authentication_error",
                    "param": null,
                    "code": "invalid_api_key"
                }
            })),
        )
            .into_response();
    }

    next.run(request).await
}

pub(crate) fn is_authorized(headers: &HeaderMap, api_key: &str) -> bool {
    let mut credentials = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = credentials.next() else {
        return false;
    };
    // Multiple credentials are ambiguous, even if one of them matches.
    if credentials.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };

    scheme.eq_ignore_ascii_case("Bearer")
        && bool::from(
            token
                .trim_start_matches(' ')
                .as_bytes()
                .ct_eq(api_key.as_bytes()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, Bytes, to_bytes};
    use axum::extract::OriginalUri;
    use axum::http::{HeaderValue, Method};
    use axum::middleware;
    use axum::routing::{any, get};
    use tower::ServiceExt;

    const KEY: &str = "sk-proxy-test-key";

    fn router(api_key: Option<&str>) -> Router {
        Router::new()
            .route("/v1/usage", get(echo_request))
            .route("/v1/{*path}", any(echo_request))
            .route_layer(middleware::from_fn_with_state(
                api_key.map(Arc::<str>::from),
                authenticate,
            ))
    }

    async fn echo_request(
        method: Method,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let mut response = body.into_response();
        response.headers_mut().insert(
            "x-test-method",
            HeaderValue::from_str(method.as_str()).unwrap(),
        );
        response.headers_mut().insert(
            "x-test-uri",
            HeaderValue::from_str(&uri.to_string()).unwrap(),
        );
        if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type.clone());
        }
        response
    }

    #[tokio::test]
    async fn rejects_missing_invalid_and_ambiguous_credentials() {
        let valid = format!("Bearer {KEY}");
        let cases = [
            vec![],
            vec![""],
            vec!["Bearer"],
            vec!["Bearer "],
            vec!["Bearer wrong-key"],
            vec!["Bearer SK-PROXY-TEST-KEY"],
            vec!["Basic sk-proxy-test-key"],
            vec!["Bearers sk-proxy-test-key"],
            vec!["Bearer\tsk-proxy-test-key"],
            vec!["Bearer sk-proxy-test-key "],
            vec!["Bearer sk-proxy-test-key, Bearer wrong-key"],
            vec![valid.as_str(), "Bearer wrong-key"],
            vec!["Bearer wrong-key", valid.as_str()],
            vec![valid.as_str(), valid.as_str()],
        ];
        for credentials in cases {
            let mut request = Request::builder().uri("/v1/models");
            for value in credentials {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = router(Some(KEY))
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["error"]["type"], "authentication_error");
            assert_eq!(error["error"]["code"], "invalid_api_key");
            assert!(error["error"]["param"].is_null());
            assert!(!String::from_utf8_lossy(&body).contains(KEY));
            assert!(!String::from_utf8_lossy(&body).contains("wrong-key"));
        }
    }

    #[tokio::test]
    async fn protects_usage_models_native_claude_and_generic_methods_before_reading_body() {
        for (method, path) in [
            (Method::GET, "/v1/usage"),
            (Method::GET, "/v1/models"),
            (Method::POST, "/v1/chat/completions"),
            (Method::POST, "/v1/responses"),
            (Method::POST, "/v1/messages"),
            (Method::POST, "/v1/messages/count_tokens"),
            (Method::GET, "/v1/messages"),
            (Method::DELETE, "/v1/files/example"),
        ] {
            let body = Body::from_stream(futures::stream::once(async {
                Err::<Bytes, _>(std::io::Error::other("body must not be read"))
            }));
            let response = router(Some(KEY))
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn accepts_bearer_scheme_case_insensitively_and_preserves_request() {
        let body = Bytes::from_static(b"\0invalid json\xff\r\n");
        let uri = "/v1/files/example?purpose=a%20b&purpose=c";
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR  "] {
            let response = router(Some(KEY))
                .oneshot(
                    Request::builder()
                        .method(Method::PATCH)
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("{scheme} {KEY}"))
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-test-method"], "PATCH");
            assert_eq!(response.headers()["x-test-uri"], uri);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/octet-stream"
            );
            assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), body);
        }
    }

    #[tokio::test]
    async fn rejects_non_ascii_header_values_and_query_string_credentials() {
        for request in [
            Request::builder()
                .uri("/v1/models")
                .header(
                    header::AUTHORIZATION,
                    HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
                )
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri(format!("/v1/models?api_key={KEY}"))
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = router(Some(KEY)).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn leaves_unconfigured_authentication_and_non_api_paths_unchanged() {
        for credential in [None, Some("Bearer ignored"), Some("Basic ignored")] {
            let mut request = Request::builder().uri("/v1/models");
            if let Some(value) = credential {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = router(None)
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        for path in ["/", "/health", "/v1", "/v1/", "/other"] {
            let response = router(Some(KEY))
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
}
