# Get a flow

One Flow run and its ordered event timeline.

## Endpoint

- **Method:** `GET`
- **URL:** `/api/flows/{flow_id}`

## Headers

| Header | Required | Description |
| --- | --- | --- |
| `Authorization` | When auth is enabled | The shared server token. A JWT is rejected with `403`. |

## Path parameters

| Parameter | Type | Required | Description |
| --- | --- | --- | --- |
| `flow_id` | string | Yes | ID from [List flows](list.md) |

## Response

### 200

```json
{
  "flow": {
    "flow_id": "…",
    "owner": "mcp",
    "workspace": "/work/project",
    "board_id": "…",
    "template_id": "…",
    "template_version": 1,
    "goal": "Ship the parser",
    "references": [],
    "stage": "plan",
    "state": "active",
    "revision": 3,
    "coordinator": null,
    "coordinator_fence": 0,
    "tasks": {},
    "pending_actions": [],
    "created_by": "agent-1",
    "created_at": "2026-10-20T16:00:00Z",
    "updated_at": "2026-10-20T16:10:00Z"
  },
  "events": [
    {
      "event_id": "…",
      "request_id": "…",
      "flow_id": "…",
      "revision": 1,
      "actor": "agent-1",
      "provider": "claude",
      "attempt_id": null,
      "kind": "flow_started",
      "task_id": null,
      "post_id": null,
      "data": {},
      "timestamp": "2026-10-20T16:00:00Z"
    }
  ]
}
```

| Field | Description |
| --- | --- |
| `flow.state` | `active`, `completed`, `cancelled`, or `failed` |
| `flow.revision` | Increases with each event. Writers pass it back as `expected_revision`. |
| `flow.coordinator_fence` | A counter that rejects writes from a coordinator that has been replaced |
| `flow.tasks` | Tasks keyed by task ID |
| `flow.pending_actions` | Actions requested but not yet consumed |

Flows created from a template also carry `flow_template`, the exact definition
pinned at creation.

### Event kinds

`flow_started`, `stage_changed`, `stage_completed`, `coordinator_claimed`,
`coordinator_handed_off`, `coordinator_released`, `task_created`,
`task_assigned`, `task_started`, `progress`, `blocked`, `ready_for_review`,
`review_approved`, `review_changes_requested`, `task_cancelled`,
`flow_completed`, `flow_cancelled`, `flow_failed`, `action_requested`,
`action_consumed`, `reference_added`.

### Errors

| Status | Meaning |
| --- | --- |
| `401` | Missing or invalid credentials |
| `403` | A JWT was used |
| `404` | `{"detail":"flow not found"}` |
| `500` | `{"detail":"flow unavailable: ..."}` |

## Example

```bash
curl -H "Authorization: Bearer $MEMBRICK_TOKEN" "http://127.0.0.1:8080/api/flows/$FLOW_ID"
```
