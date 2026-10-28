# Integrations

Provider readiness for the agent integrations compiled into this server.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/integrations`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{
  "integrations": [],
  "note": "Provider hooks and MCP processes report independently; this server does not infer agent connectivity from index health."
}
```

Each entry carries the provider name, whether it is compiled in, its state, and
its event and session counters, the same fields as the `providers` array in
[Metrics (JSON)](metrics.md) plus its recent events.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/integrations
```
