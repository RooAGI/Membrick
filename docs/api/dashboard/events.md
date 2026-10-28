# Recent events

The latest sanitized provider lifecycle events across all sessions.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/events`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{ "events": [] }
```

Events are sanitized and bounded. They include the provider, an opaque
`session_key`, the event name, and a timestamp, and no prompt or tool content.
See [Telemetry](../../telemetry.md#provider-lifecycle-events).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/events
```
