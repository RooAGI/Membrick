# Extension change stream

An authenticated, workspace-scoped server-sent event stream for change notices
from registered extensions. Membrick forwards each notice as opaque JSON and
does not interpret its contents.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/extensions/events`
- **Content-Type (response):** `text/event-stream`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |
| `Accept` | No | `text/event-stream` |

## Events

| Event | Data | Meaning |
| --- | --- | --- |
| `ready` | `{}` | The stream is open |
| `change` | Opaque extension JSON | An extension-provided change notice |
| `resync` | `{"skipped": n}` | The client fell behind; refetch the view |

A keep-alive comment is sent every 15 seconds. Change payloads are provided by
the owning extension's `change_notification` hook and may have extension-
specific schemas.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `503` | `{"detail":"extension events unavailable"}`. The writer is not available. |

## Example

```bash
curl -N -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/extensions/events
```
