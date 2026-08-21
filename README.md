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
- Background Copilot token refresh
- User-level service installation

## Requirements

- A GitHub account with an active Copilot subscription
- A Rust toolchain when building from source

## Installation

```bash
cargo build --release
```

The binary is written to `target/release/copilot-api-proxy`.

## Quick Start

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

## API Surface

| Route | Method | Behavior |
|---|---|---|
| `/v1/usage` | `GET` | Returns the current Copilot usage response. |
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
| `ANTHROPIC_API_KEY` | Optional client-facing key required on native `/v1/messages*` routes. | Unset |
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

# Uninstall
copilot-api-proxy service uninstall
```

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
