# Flows

These read-only routes are contributed by the Flowboard FlowRun extension and
used by the dashboard. Agents create and advance runs through the
[FlowRun extension tools](../../extensions/flowrun.md). Requests need the
shared server token.

| Route | Page |
| --- | --- |
| `GET /api/flows` | [List flows](list.md) |
| `GET /api/flows/{flow_id}` | [Get a flow](get.md) |

To follow changes live, subscribe to the [generic extension event
stream](../extensions/events.md).
