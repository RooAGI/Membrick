# Unreleased

## Compatibility changes

- A REST add that reuses a `request_id` with different content or session now
  returns HTTP `409 Conflict`; earlier server versions returned `422
  Unprocessable Entity`. REST clients that branch on the status code must
  handle `409`. Rust, Python, and Node clients continue to classify either
  status as a rejected 4xx response.

- When an explicit `request_id` is retried after upgrading from a version that
  generated a random session for each attempt, the server can recover the old
  session if the original write remains in the same store and its messages and
  metadata match exactly. It returns the original session without duplicating
  the write. Changed payloads and different explicit sessions still conflict.
