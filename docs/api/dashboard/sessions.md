# List sessions

Recent agent sessions with sanitized lifecycle counts.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/sessions`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

```json
{
  "sessions": [
    {
      "provider": "claude",
      "session_key": "opaque-key",
      "event_count": 12,
      "first_seen_ms": 1760000000000,
      "last_seen_ms": 1760000900000,
      "last_event": "agent_turn_end"
    }
  ]
}
```

| Field | Description |
| --- | --- |
| `provider` | The agent integration |
| `session_key` | An opaque telemetry key, not a raw session ID |
| `event_count` | Lifecycle events seen for the session |
| `first_seen_ms`, `last_seen_ms` | Unix milliseconds |
| `last_event` | Name of the most recent event |

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/sessions
```
