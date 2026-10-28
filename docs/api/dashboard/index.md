# Dashboard telemetry

Read-only routes that feed the local [dashboard](../../observability.md). They
report on the whole workspace, so they accept only the shared server token. A
JWT gets `403`. Telemetry streams, retention, and privacy are described in
[Telemetry](../../telemetry.md).

| Route | Page |
| --- | --- |
| `GET /api/status` | [Status](status.md) |
| `GET /api/timeseries` | [Timeseries](timeseries.md) |
| `GET /api/metrics` | [Metrics (JSON)](metrics.md) |
| `GET /api/integrations` | [Integrations](integrations.md) |
| `GET /api/sessions` | [List sessions](sessions.md) |
| `GET /api/sessions/{session_key}/events` | [Session events](session-events.md) |
| `GET /api/events` | [Recent events](events.md) |
| `GET /api/search-reviews` | [Search reviews](search-reviews.md) |
| `GET /api/captures` | [Capture status](captures.md) |
