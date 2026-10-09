# FerrisRBE MCP

FerrisRBE serves an MCP server at `/mcp` on its HTTP port: 18 read-only tools
over the build data (`rbe_builds`, `rbe_build`, `rbe_build_events`,
`rbe_build_log`, `rbe_build_profile`, `rbe_build_misses`, `rbe_build_targets`,
`rbe_build_inputs`, `rbe_targets`, `rbe_target_history`, `rbe_tests`,
`rbe_test_history`, `rbe_workers`, `rbe_externals`, `rbe_stats`,
`rbe_metrics`, `rbe_blob`, `rbe_prometheus`). The transport is streamable
HTTP, stateless, with JSON responses.

`rbe_metrics` shows the counters of the running server, which restart with
it. `rbe_prometheus` runs PromQL against the cluster's Prometheus
(`RBE_PROMETHEUS_URL`), which keeps their history: the p90 queue wait over a
week, cancelled operations per day.

With `RBE_AUTH_ENABLED=true` every request needs `Authorization: Bearer
<token>`: a JWT of the Keycloak realm (from a client in
`RBE_AUTH_ALLOWED_CLIENTS`, when set) or one of `RBE_AUTH_STATIC_TOKENS`. The
server also refuses any `Host` other than `localhost`, `127.0.0.1`, `::1` and
`RBE_MCP_ALLOWED_HOSTS` (the public host, `rbe.example.com` below), and any
request carrying an `Origin` header, so a web page cannot call it.

## Two ways in

| From                         | URL                           | Credential                                                                      |
| ---------------------------- | ----------------------------- | ------------------------------------------------------------------------------- |
| A laptop with cluster access | `http://localhost:18080/mcp`  | none in the client: `rbe-cli connect` adds your Keycloak token and refreshes it |
| Anything else (headless)     | `https://rbe.example.com/mcp` | `Authorization: Bearer` with a static token                                     |

Prefer the first: the token is short-lived, tied to you, and never lands in
a client config. Start the proxy once per session (`login` reads
`RBE_KEYCLOAK_URL`, `RBE_KEYCLOAK_REALM` and `RBE_KEYCLOAK_CLIENT_ID`):

```bash
bazel run //:cli -- login     # once, or when the refresh token dies
bazel run //:cli -- connect   # keep it running
```

## Claude Code

Claude Code speaks streamable HTTP natively:

```bash
# Through rbe-cli connect
claude mcp add --transport http --scope user rbe http://localhost:18080/mcp

# Headless, with a static token (stored in ~/.claude.json)
claude mcp add --transport http --scope user rbe https://rbe.example.com/mcp \
  -H "Authorization: Bearer $RBE_MCP_TOKEN"
```

`claude mcp list` shows `rbe: ... (HTTP) - ✔ Connected` when it works. The
tools load when a session starts.

## opencode

opencode's `remote` client has failed the handshake with this server in use
(the cause is not investigated yet), so go through
[`mcp-remote`](https://www.npmjs.com/package/mcp-remote) as a local (stdio)
server:

```bash
mkdir -p ~/.config/opencode/mcp-rbe && cd ~/.config/opencode/mcp-rbe
npm install mcp-remote
```

```jsonc
// ~/.config/opencode/opencode.jsonc
{
  "mcp": {
    "rbe": {
      "type": "local",
      "command": [
        "/Users/<you>/.config/opencode/mcp-rbe/node_modules/.bin/mcp-remote",
        "http://localhost:18080/mcp",
        "--allow-http",
      ],
      "enabled": true,
    },
  },
}
```

Headless, replace the last two arguments with `https://rbe.example.com/mcp`,
`--header` and `Authorization: Bearer <token>`; keep the token in a file
outside the config and read it from a wrapper script.

## Any other client

A client with streamable HTTP takes the URL and, for the public endpoint, the
`Authorization` header. A stdio-only client runs `mcp-remote <url>` as its
command, as opencode does above.

## Checking the endpoint

```bash
curl -s -X POST http://localhost:18080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

| Answer                                  | Cause                                                                       |
| --------------------------------------- | --------------------------------------------------------------------------- |
| `200` with `serverInfo`                 | working                                                                     |
| `401`                                   | no or expired token: `bazel run //:cli -- login`, or check the static token |
| `403 Host header is not allowed`        | a host outside `RBE_MCP_ALLOWED_HOSTS`                                      |
| `403 Origin header is not allowed`      | the client sends `Origin` (a browser); MCP clients must not                 |
| connection refused on `localhost:18080` | `rbe-cli connect` is not running                                            |
