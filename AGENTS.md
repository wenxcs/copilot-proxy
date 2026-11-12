# AGENTS.md

## Project Overview

`copilot-api-proxy` is a Rust reverse proxy around GitHub Copilot with one HTTP route family: OpenAI-compatible `/v1/*` endpoints, including native Claude handling on `/v1/messages*`.

## Design Principles

- Keep generic `/v1/*` requests as raw-byte passthrough whenever possible.
- Forward native Claude requests directly to Copilot without converting protocols.
- Accept only native Claude model families on Anthropic routes. Other models must use an OpenAI-compatible route.
- Inspect only chat-completion, Responses, and native Claude request bodies for sticky initiator and vision headers. Configured model policies may additionally inspect top-level model identifiers in generic JSON requests.
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

# Requires Authorization: Bearer <key> on API routes (optional)
export COPILOT_PROXY_API_KEY=your-secret-key

# Optional ordered model access rules and fixed-window token allowances
export COPILOT_PROXY_MODEL_RULES='-all,+gpt-6-astra,5h:gpt-6-astra:1024,7d:gpt-6-astra:1024'

# Enables the separate admin limits interface
export COPILOT_PROXY_SUPER_PASSWORD=your-admin-secret

# Optional absolute path for persistent counters
export COPILOT_PROXY_LIMIT_STATE_PATH=/path/to/model_usage.json

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
    +-- /v1/admin/limits*
    |      '-- inspect/reset local token limits using the super password
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
├── client_auth.rs # Optional incoming bearer-token authentication
├── config.rs     # Token and VS Code identity storage
├── policy_config.rs # Environment and private-file model policy configuration
├── model_policy.rs # Model access rules and persistent fixed-window accounting
├── usage.rs      # JSON and incremental SSE token usage observation
├── metered_response.rs # Token accounting and errors before terminal SSE events
├── server.rs     # Axum state, router, and /v1 dispatch
├── llm.rs        # OpenAI and native Claude route handlers
├── claude.rs     # Native Claude validation and normalization
├── initiator.rs  # Sticky initiator and vision detection
├── proxy.rs      # Copilot HTTP client and response forwarding
└── error.rs      # Shared internal error responses
```

## Critical Implementation Details

### Client Authentication

An optional `COPILOT_PROXY_API_KEY` requires `Authorization: Bearer <key>` on proxy API routes, including usage and native Claude routes. Admin limit routes use a separate super password. `server --api-key`, `server --api-key-file`, and `service install --api-key` override the environment. The two server key flags conflict. Explicit key files must exist; only trailing CR/LF characters are stripped. Keys must be nonempty and contain only ASCII letters, digits, or `-._~+/=`; invalid configured keys are rejected, and an unset key preserves unauthenticated access. Invalid or missing client credentials return `401` JSON with `WWW-Authenticate: Bearer` before handlers inspect request bodies. Non-API paths retain normal `404` responses.

The proxy API key is separate from GitHub OAuth and is never forwarded upstream. Service installation saves it in `~/.local/share/copilot-api-proxy/proxy_api_key` with mode `0600`, passes only its path in service arguments, and restarts an existing active service so changes take effect. Token files are created privately and existing permissions are restricted before writing secrets. Compose forwards the environment variable from the shell or `.env`.

### Model Access and Token Limits

`COPILOT_PROXY_MODEL_RULES` accepts comma/whitespace-separated `-all`, `+all`, `+model`, `-model`, `5h:model:tokens`, and `7d:model:tokens`. Access rules apply in order; `+all`/`-all` clear individual overrides. Quotas do not implicitly allow a denied model. Invalid rules fail startup; unset rules preserve unrestricted model access.

Quotas count combined upstream input/output usage in fixed per-model windows anchored to first metered use, with independent 18,000-second and 604,800-second periods. Counters survive restart in a private `model_usage.json` beside the GitHub token; `COPILOT_PROXY_LIMIT_STATE_PATH` overrides this with an absolute path. Model denial returns `403`, exhausted allowance returns `429` with retry timing, and concurrent requests for the same limited model return `429`. Actual usage can exceed the remaining allowance within one response; missing usage or interrupted metered responses exhaust remaining allowances to prevent bypass.

Metered chat streams request `stream_options.include_usage=true`. Native count-token routes check access without charging usage. Limited background Responses requests are rejected. Post-header streaming failures use SSE error events before protocol terminal events rather than changing the HTTP status. The ledger uses a process-lifetime file lock and atomic private-file writes; invalid state fails startup.

`GET /v1/admin/limits` and `POST /v1/admin/limits/reset` require `Authorization: Bearer <COPILOT_PROXY_SUPER_PASSWORD>` independently of normal client authentication. The endpoints return `404` when the super password is unset. Empty/non-printable/whitespace passwords fail startup without logging the secret. Reset clears counters and invalidates accounting from existing requests while preserving rules/caps. Reset timestamps are Unix seconds.

`server --model-policy-file PATH` reads complete JSON policy configuration instead of policy environment variables. Service installation stores configured rules, state path, and the super password in private `model_policy_config.json` and passes only its path. Compose forwards all three policy variables.

### Generic `/v1/*` Passthrough

`src/server.rs` captures `/v1/{*path}`. `src/llm.rs` strips the leading `/v1`, preserves the query string and raw body, and forwards the request to the API base returned by Copilot's token exchange.

Only these OpenAI-compatible paths receive initiator and vision analysis:

- `POST /v1/chat/completions`: inspect `messages`
- `POST /v1/responses`: inspect `input`

Without model policies, all other generic paths are forwarded without body parsing. Configured policies also inspect top-level model identifiers on JSON POST/PUT/PATCH requests. Opaque bodies on other paths retain raw-byte passthrough. Only limited chat streams rewrite their JSON body to request usage metadata.

### Native Claude Routes

`POST /v1/messages` and `POST /v1/messages/count_tokens` are handled as native Claude requests. The handler:

1. Requires a Claude-family model name.
2. Infers initiator and vision headers from `messages`.
3. Normalizes unsupported request hints while preserving the Anthropic protocol.
4. Forwards directly to Copilot's `/v1/messages*` endpoint.

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
