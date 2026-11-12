//! Error types with OpenAI-compatible response formatting.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Upstream request failed: {0}")]
    Upstream(#[from] reqwest::Error),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Model '{0}' is disallowed by the proxy model policy")]
    ModelDenied(String),
    #[error("A metered request for model '{0}' is already in progress; retry after it completes")]
    ModelBusy(String),
    #[error(
        "{window} token limit exceeded for model '{model}': {used} tokens used, limit {limit}; retry in {retry_after} seconds"
    )]
    LimitExceeded {
        model: String,
        window: String,
        limit: u64,
        used: u64,
        retry_after: u64,
    },
    #[error(
        "Upstream token usage could not be verified; the remaining model budget has been exhausted. Contact the administrator to reset limits"
    )]
    UsageUnavailable,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn status_and_type(&self) -> (StatusCode, &'static str) {
        match self {
            Error::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication_error"),
            Error::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error"),
            Error::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
            Error::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
            Error::ModelDenied(_) => (StatusCode::FORBIDDEN, "permission_error"),
            Error::ModelBusy(_) | Error::LimitExceeded { .. } => {
                (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
            }
            Error::UsageUnavailable => (StatusCode::BAD_GATEWAY, "upstream_error"),
            Error::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "io_error"),
        }
    }

    pub(crate) fn retry_after(&self) -> Option<u64> {
        match self {
            Self::LimitExceeded { retry_after, .. } => Some(*retry_after),
            Self::ModelBusy(_) => Some(1),
            _ => None,
        }
    }

    pub(crate) fn body(&self) -> serde_json::Value {
        let code = match self {
            Self::ModelDenied(_) => Some("model_not_allowed"),
            Self::ModelBusy(_) => Some("model_request_in_progress"),
            Self::LimitExceeded { .. } => Some("model_token_limit_exceeded"),
            Self::UsageUnavailable => Some("usage_unavailable"),
            _ => None,
        };
        let mut body = serde_json::json!({
            "error": {
                "message": self.to_string(),
                "type": self.status_and_type().1,
                "param": null,
                "code": code
            }
        });
        if let Self::LimitExceeded {
            model,
            window,
            limit,
            used,
            retry_after,
        } = self
        {
            body["error"]["model"] = model.as_str().into();
            body["error"]["window"] = window.as_str().into();
            body["error"]["limit"] = (*limit).into();
            body["error"]["used"] = (*used).into();
            body["error"]["retry_after"] = (*retry_after).into();
        }
        body
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (self.status_and_type().0, Json(self.body())).into_response();
        if let Some(retry_after) = self.retry_after() {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                retry_after.to_string().parse().expect("valid seconds"),
            );
        }
        response
    }
}
