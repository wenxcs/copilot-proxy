//! copilot-api-proxy - A reverse proxy for GitHub Copilot API

pub mod auth;
pub mod claude;
mod client_auth;
pub mod config;
pub mod error;
pub mod initiator;
pub mod llm;
mod metered_response;
mod model_policy;
pub mod policy_config;
pub mod proxy;
pub mod server;
mod usage;

pub use error::Error;
