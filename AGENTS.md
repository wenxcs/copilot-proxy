# AGENTS.md

## Project Overview

`copilot-api-proxy` is a Rust reverse proxy around GitHub Copilot with one HTTP route family: OpenAI-compatible `/v1/*` endpoints, including native Claude handling on `/v1/messages*`.

## Design Principles

- Keep generic `/v1/*` requests as raw-byte passthrough whenever possible.
- Forward native Claude requests directly to Copilot without converting protocols.
- Accept only native Claude model families on Anthropic routes. Other models must use an OpenAI-compatible route.
- Inspect only chat-completion, Responses, and native Claude request bodies for sticky initiator and vision headers.
- Keep authentication, token refresh, and response forwarding independent from request schemas.

## Common Commands

### Build and Run

```bash
cargo build
cargo build --release
cargo run -- server
cargo run -- server --port 8080
cargo run -- server --log-level debug
cargo run -- auth
cargo run -- service install
cargo run -- service uninstall
```

### Test

```bash
cargo fmt --check
cargo test
cargo test -- --nocapture
cargo test test_name
```

### Manual Endpoint Checks

```bash
# OpenAI passthrough
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o-mini-2024-07-18","messages":[{"role":"user","content":"Hello"}]}'

# Native Claude passthrough
curl -X POST http://localhost:9876/v1/messages \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4.6","max_tokens":256,"messages":[{"role":"user","content":"Hello"}]}'

# Models and usage
curl http://localhost:9876/v1/models
curl http://localhost:9876/v1/usage
```

## Environment Variables

```bash
# Overrides the stored GitHub token
export GITHUB_TOKEN=your_github_token

# Optional client-facing key for native Claude routes
export ANTHROPIC_API_KEY=your_client_api_key

# Overrides logging completely
export RUST_LOG=copilot_api_proxy=debug,tower_http=debug
```

## Request Flow

```text
Client request
    |
    v
Axum router
    |
    +-- GET /v1/usage
    |      '-- fetch Copilot usage with the GitHub token
    |
    '-- /v1/{*path}
           |
           +-- /v1/messages* -> validate and forward native Claude request
           '-- everything else -> generic Copilot passthrough
```

All non-`/v1` paths return the router's normal `404 Not Found` response.

## Module Structure

```text
src/
├── main.rs       # CLI entry point and service management
├── lib.rs        # Library exports
├── auth.rs       # GitHub device flow and Copilot token manager
├── config.rs     # Token and VS Code identity storage
├── server.rs     # Axum state, router, and /v1 dispatch
├── llm.rs        # OpenAI and native Claude route handlers
├── claude.rs     # Native Claude validation and normalization
├── initiator.rs  # Sticky initiator and vision detection
├── proxy.rs      # Copilot HTTP client and response forwarding
└── error.rs      # Shared internal error responses
```

## Critical Implementation Details

### Generic `/v1/*` Passthrough

`src/server.rs` captures `/v1/{*path}`. `src/llm.rs` strips the leading `/v1`, preserves the query string and raw body, and forwards the request to the API base returned by Copilot's token exchange.

Only these OpenAI-compatible paths are inspected:

- `POST /v1/chat/completions`: inspect `messages`
- `POST /v1/responses`: inspect `input`

All other generic paths are forwarded without body parsing.

### Native Claude Routes

`POST /v1/messages` and `POST /v1/messages/count_tokens` are handled as native Claude requests. The handler:

1. Optionally validates the client key when `ANTHROPIC_API_KEY` is configured.
2. Requires a Claude-family model name.
3. Infers initiator and vision headers from `messages`.
4. Normalizes unsupported request hints while preserving the Anthropic protocol.
5. Forwards directly to Copilot's `/v1/messages*` endpoint.

Other methods return an Anthropic-shaped `400 Bad Request` response.

### Sticky Inference

- A prior `assistant` or `tool` turn produces `X-Initiator: agent`.
- User-only or invalid JSON produces `X-Initiator: user`.
- OpenAI `image_url`, Responses `input_image`, and Anthropic `image` parts set `Copilot-Vision-Request: true`.

### Copilot Headers

Every Copilot request injects the VS Code Copilot identity headers defined in `src/proxy.rs`, plus per-request identity, `X-Initiator`, and the optional vision header.

### Token Lifecycle

GitHub token loading order:

1. `GITHUB_TOKEN`
2. `~/.local/share/copilot-api-proxy/github_token`

`TokenManager` exchanges the GitHub token on startup, refreshes the Copilot token in the background before expiry, retries failed refreshes, and force-refreshes once after an upstream `401`.

### Response Forwarding

`forward_response()` preserves status and end-to-end headers while removing:

- `transfer-encoding`
- `connection`
- `keep-alive`
- `proxy-authenticate`
- `proxy-authorization`
- `te`
- `trailers`
- `upgrade`

SSE responses receive `Cache-Control: no-cache` when the upstream omits it.

### Server Limits

- Request body limit: 10 MiB
- Upstream request timeout: 300 seconds
- Request tracing: method, URI, resolved initiator, and upstream path

## Authentication Flow

1. `cargo run -- auth` starts GitHub's OAuth device flow.
2. The CLI prints the verification URL and code.
3. After authorization, the token is stored with restrictive Unix permissions.
4. Server startup exchanges it through `GET https://api.github.com/copilot_internal/v2/token`.

## Troubleshooting

### Address Already in Use

Choose another port:

```bash
cargo run -- server --port 8080
```

### GitHub Token Not Found or Invalid

Run the device flow again:

```bash
cargo run -- auth
```

### Upstream or Model Errors

Enable debug logging and inspect the upstream model list:

```bash
cargo run -- server --log-level debug
curl http://localhost:9876/v1/models
```

`/v1/responses` requires a model that supports the Responses API. Native `/v1/messages*` routes require a Claude-family model exposed by Copilot.
