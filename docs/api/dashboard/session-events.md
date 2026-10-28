# Session events

Sanitized lifecycle events for one session.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/sessions/{session_key}/events`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Path parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `session_key` | string | Yes | The opaque key from [List sessions](sessions.md) |

## Response

### 200

```json
{ "session_key": "opaque-key", "events": [] }
```

An unknown key returns an empty `events` array, not an error.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" \
  "http://127.0.0.1:8080/api/sessions/$SESSION_KEY/events"
```
