# tau-ext-websearch

A Tau extension that registers generic web search tools. The existing Exa-backed
search remains enabled by default, and Parallel.ai search/fetch tools are
registered in the same extension but disabled by default for role-level opt-in.

## Tools

- `websearch_exa`, advertised to models as `web_search`, proxies Exa's keyless
  hosted MCP at <https://mcp.exa.ai/mcp>.
- `websearch_parallel_search`, advertised to models as `web_search`, proxies the
  default unauthenticated Parallel Search MCP endpoint at
  <https://search.parallel.ai/mcp>. This tool is disabled by default to avoid a
  duplicate model-visible `web_search` unless a role explicitly enables it.
- `websearch_parallel_fetch`, advertised to models as `web_fetch`, fetches and
  extracts a page through the same unauthenticated Parallel MCP endpoint. This
  tool is also disabled by default.

No Parallel API key is supported: there is no `api_key` config, and the
extension does not send an Authorization header.

## Configuration

The built-in extension is `std-websearch` and is enabled by default. Disable it
if you'd rather not make outbound HTTP calls:

```json5
{
  extensions: {
    "std-websearch": { enable: false },
  },
}
```

Endpoint overrides:

```json5
{
  extensions: {
    "std-websearch": {
      config: {
        // Backwards-compatible alias for exa_endpoint.
        endpoint: "https://mcp.exa.ai/mcp?exaApiKey=sk-…",
        // Or explicitly:
        exa_endpoint: "https://mcp.exa.ai/mcp",
        parallel_endpoint: "https://search.parallel.ai/mcp",
      },
    },
  },
}
```

## Tracing

```sh
TAU_EXT_LOG=websearch=debug tau …
```
