# List flows

Every Flow in the workspace. The list is not paginated.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/flows`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Response

### 200

Flows in ascending `flow_id` order. Each entry is a snapshot: the run and its
ordered events.

```json
{
  "flows": [
    {
      "run": { "flow_id": "…", "stage": "plan", "state": "active", "revision": 3 },
      "events": []
    }
  ]
}
```

The `run` and `events` shapes are described in [Get a flow](get.md).

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `500` | `{"detail":"flows unavailable: ..."}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" http://127.0.0.1:8080/api/flows
```
