# copilot-api-proxy

A Rust reverse proxy that exposes GitHub Copilot through OpenAI-compatible `/v1/*` routes and native Claude `/v1/messages` passthrough. Requests are forwarded to Copilot without cross-protocol conversion.

> [!WARNING]
> This is a reverse-engineered proxy of the GitHub Copilot API. It is not supported by GitHub and may break unexpectedly. Use it at your own risk.

> [!WARNING]
> Excessive automated or bulk use may trigger GitHub's abuse-detection systems and could restrict your Copilot access. Review the [GitHub Acceptable Use Policies](https://docs.github.com/site-policy/acceptable-use-policies/github-acceptable-use-policies#4-spam-and-inauthentic-activity-on-github) and [GitHub Copilot Terms](https://docs.github.com/site-policy/github-terms/github-terms-for-additional-products-and-features#github-copilot).

## Features

- Raw-byte passthrough for OpenAI-compatible `/v1/*` routes
- Native Claude `/v1/messages` and `/v1/messages/count_tokens` passthrough
- Claude models on Copilot's OpenAI-compatible routes
- Streaming, tool/function calling, and vision support
- Sticky `X-Initiator` inference for multi-turn requests
- GitHub OAuth device-flow authentication
- Optional OpenAI-style bearer-token authentication for clients
- Model allow/deny rules and persistent per-model token limits
- Password-protected limit inspection and reset endpoints
- Background Copilot token refresh
- User-level service installation

## Requirements

- A GitHub account with an active Copilot subscription
- Docker with Docker Compose, or a Rust toolchain when building from source

## Build from Source

```bash
cargo build --release
```

The binary is written to `target/release/copilot-api-proxy`.

## Local Binary Quick Start

Authenticate once:

```bash
copilot-api-proxy auth
```

The device flow stores the GitHub token at `~/.local/share/copilot-api-proxy/github_token`.

Start the proxy:

```bash
# Default address: 0.0.0.0:9876
copilot-api-proxy server

# Custom port
copilot-api-proxy server --port 8080

# Debug logging
copilot-api-proxy server --log-level debug
```

Use `http://localhost:9876/v1` as the base URL for an OpenAI-compatible client. Native Claude clients can use `http://localhost:9876` so their requests reach `/v1/messages`.

## Client API Key

Set a proxy API key to require authentication:

```bash
export COPILOT_PROXY_API_KEY=your-secret-key
copilot-api-proxy server
```

Alternatively, use `copilot-api-proxy server --api-key your-secret-key` or `copilot-api-proxy server --api-key-file /path/to/private-key`. These flags override `COPILOT_PROXY_API_KEY` and cannot be combined. Key files may end with a newline. Missing files and invalid configured keys prevent startup. Keys must be nonempty and contain only ASCII letters, digits, or `-._~+/=`. Without a configured key, the proxy accepts requests without client authentication.

Configure an OpenAI-compatible client with this key, or send it directly:

```bash
curl http://localhost:9876/v1/models \
  -H "Authorization: Bearer $COPILOT_PROXY_API_KEY"
```

Client authentication covers proxy API routes, including usage and native Claude endpoints. Native Claude clients must also send `Authorization: Bearer <key>`. Missing or invalid credentials return `401` with a JSON error and `WWW-Authenticate: Bearer`. Admin limit endpoints use the separate super password described below. Neither client keys nor the super password are forwarded to Copilot.

## Model Access and Token Limits

Configure rules and an optional admin super password before starting the server:

```bash
export COPILOT_PROXY_MODEL_RULES='-all,+gpt-6-astra,5h:gpt-6-astra:1024,7d:gpt-6-astra:1024'
export COPILOT_PROXY_SUPER_PASSWORD='your-admin-secret'
copilot-api-proxy server
```

Rules accept comma or whitespace separators:

| Rule | Effect |
|---|---|
| `-all` | Disallow every model and clear individual allow/deny overrides. |
| `+all` | Allow every model and clear individual allow/deny overrides. |
| `+gpt-6-astra` | Allow this exact model ID. |
| `-gpt-6-astra` | Disallow this exact model ID. |
| `5h:gpt-6-astra:1024` | Set a 1,024-token allowance for each 5-hour window. |
| `7d:gpt-6-astra:1024` | Set a 1,024-token allowance for each 7-day window. |

Access rules apply in order, so the last applicable rule wins. Setting a token limit does not allow an otherwise disallowed model. Without rules, all models remain allowed and no local token limits apply. Invalid rules prevent startup.

Limits count combined input and output tokens reported by upstream, including cached input tokens. Each model has independent fixed windows anchored to its first metered request: 5 hours is 18,000 seconds and 7 days is 604,800 seconds. Requests within a window do not extend it. Both configured allowances must remain available. Denied models return `403`; exhausted limits return `429` with a JSON error and retry timing. A limited model accepts one request at a time; overlapping requests return `429` and can be retried after the current request completes.

Policies check model identifiers on JSON requests, including chat completions, Responses, and native Claude. Native token-count requests check model access without consuming the allowance. Limited chat streams automatically request upstream usage metadata. Background Responses requests are rejected for limited models because their final usage cannot be verified in the initial response. If a limit error is discovered after streaming headers were sent, the HTTP status remains `200` and an SSE `error` event appears before the terminal event.

Actual usage is known after upstream processes a request, so one response can exceed the remaining allowance. If a successful response has no usable usage data, or its stream is interrupted, the remaining allowance is exhausted to prevent uncounted requests. Usage counters persist across restarts in `~/.local/share/copilot-api-proxy/model_usage.json` with private file permissions. Set `COPILOT_PROXY_LIMIT_STATE_PATH` to an absolute file path to use another location; for Docker, this must be a container path on persistent storage.

Only one proxy process can use a counter file at a time. Corrupt state prevents startup instead of silently clearing allowances.

Inspect or reset all model counters with the super password:

```bash
curl http://localhost:9876/v1/admin/limits \
  -H "Authorization: Bearer $COPILOT_PROXY_SUPER_PASSWORD"

curl -X POST http://localhost:9876/v1/admin/limits/reset \
  -H "Authorization: Bearer $COPILOT_PROXY_SUPER_PASSWORD"
```

These admin endpoints require the super password independently of the client API key. They return `404` when `COPILOT_PROXY_SUPER_PASSWORD` is unset, and invalid admin credentials return `401`. The password must be nonempty printable ASCII without whitespace. Reset clears usage, including accounting from requests already in flight; it preserves model access rules and token allowances. Reported reset times are Unix timestamps in seconds.

`server --model-policy-file /path/to/private-config.json` loads the complete policy configuration from a JSON file instead of the three policy environment variables. Service installation creates such a private file automatically.

## Docker Compose

Docker Compose builds the release binary in a multi-stage image, so Rust is not required on the host.

Build the image, then run the GitHub device flow inside a one-off container:

```bash
docker compose build
docker compose run --rm copilot-proxy auth
```

Open the GitHub URL printed by the second command, enter the displayed code, and leave the command running until it reports `Authentication successful`.

Start the proxy and verify that it can reach Copilot:

```bash
docker compose up -d
curl --fail --show-error http://127.0.0.1:9876/v1/models
```

The server listens on `0.0.0.0:9876` inside the container. Compose maps it to `127.0.0.1:9876` on the host by default. To use a different host port, set `COPILOT_PROXY_PORT` when starting the service:

```bash
export COPILOT_PROXY_PORT=8080
docker compose up -d
```

The `copilot-home` named volume stores the GitHub token and generated device identity. They survive container replacement and `docker compose down`. Running `docker compose down --volumes` deletes that data and requires authentication again.

If you authenticate again while the server is already running, restart it so it loads the new token:

```bash
docker compose run --rm copilot-proxy auth
docker compose restart copilot-proxy
```

View logs or stop the service with:

```bash
docker compose logs -f copilot-proxy
docker compose down
```

`RUST_LOG`, `COPILOT_PROXY_API_KEY`, and the three model policy environment variables are passed into the container when set in the shell or the project's `.env` file. To enable authentication or limits, set their variables before `docker compose up -d`. Compose intentionally uses the persisted device-flow token instead of forwarding the host's `GITHUB_TOKEN`. The host binding can be changed with `COPILOT_PROXY_BIND`.

## API Surface

| Route | Method | Behavior |
|---|---|---|
| `/v1/usage` | `GET` | Returns the current Copilot usage response. |
| `/v1/admin/limits` | `GET` | Inspects model access and token counters using the super password. |
| `/v1/admin/limits/reset` | `POST` | Clears all local token counters using the super password. |
| `/v1/messages` | `POST` | Validates and forwards native Claude requests directly to Copilot. Non-Claude models return `400 Bad Request`. |
| `/v1/messages/count_tokens` | `POST` | Forwards native Claude token-count requests directly to Copilot. |
| `/v1/{*path}` | Any | Strips the leading `/v1` and forwards the request to the Copilot API. Chat-completion and Responses requests receive initiator and vision analysis. |

All other paths return Axum's normal `404 Not Found` response.

## Usage Examples

### OpenAI Chat Completions

```bash
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini-2024-07-18",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### OpenAI Responses API

```bash
curl -X POST http://localhost:9876/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-5", "input": "Hello"}'
```

### Claude Through OpenAI Chat Completions

```bash
curl -X POST http://localhost:9876/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-sonnet-4.6",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### Native Claude Messages

```bash
curl -X POST http://localhost:9876/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-4.6",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### List Models

```bash
curl http://localhost:9876/v1/models
```

## Configuration

| Variable | Description | Default |
|---|---|---|
| `GITHUB_TOKEN` | Overrides the stored GitHub token. | Token file |
| `COPILOT_PROXY_API_KEY` | Requires this bearer token on API routes; overridden by `--api-key` or `server --api-key-file`. | Unset (authentication disabled) |
| `COPILOT_PROXY_MODEL_RULES` | Ordered model access rules and per-model `5h`/`7d` token allowances. | Unset (all allowed, no local limits) |
| `COPILOT_PROXY_SUPER_PASSWORD` | Bearer password for limit inspection and reset endpoints. | Unset (admin endpoints disabled) |
| `COPILOT_PROXY_LIMIT_STATE_PATH` | Absolute path for persistent local usage counters. | `~/.local/share/copilot-api-proxy/model_usage.json` |
| `RUST_LOG` | Overrides the logging filter. | Unset |

Token loading order:

1. `GITHUB_TOKEN`
2. `~/.local/share/copilot-api-proxy/github_token`

The token directory is created with mode `0700` and the token file with mode `0600` on Unix.

## System Service

```bash
# Install for the current user
copilot-api-proxy service install

# Install on a custom address
copilot-api-proxy service install --host 0.0.0.0 --port 8080

# Install with client authentication
copilot-api-proxy service install --api-key your-secret-key

# Uninstall
copilot-api-proxy service uninstall
```

Service installation also reads `COPILOT_PROXY_API_KEY`, with `--api-key` taking precedence. It saves the key at `~/.local/share/copilot-api-proxy/proxy_api_key` with mode `0600`; the service configuration contains only the file path. Reinstall the service to change its key and restart the running service.

Model rules, the super password, and an optional usage-state path are captured from their environment variables during installation. They are saved in `~/.local/share/copilot-api-proxy/model_policy_config.json` with mode `0600`; only the file path appears in the service definition. Reinstall the service after changing these settings.

## How It Works

1. `auth` runs GitHub's OAuth device flow and stores the GitHub token locally.
2. The server exchanges that token for a short-lived Copilot API token.
3. `TokenManager` refreshes the Copilot token in the background before expiry.
4. `/v1/*` requests are forwarded with the headers expected by Copilot.
5. Native Claude requests remain in Anthropic format and are sent directly to Copilot's native endpoint.

For chat-completion and Responses requests, prior `assistant` or `tool` turns set `X-Initiator: agent`; otherwise it is `user`. Image inputs also set `Copilot-Vision-Request: true`.

The server limits request bodies to 10 MiB and strips hop-by-hop headers from upstream responses.

## Development

```bash
cargo fmt --check
cargo test
```

## License

MIT
