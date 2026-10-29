use super::*;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/extensions/events", get(api_extension_events))
}

pub(super) async fn api_extension_events(
    State(state): State<AppState>,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::StreamExt;
    use tokio::sync::broadcast::error::RecvError;

    let Some(writer) = state.staged_writer.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail":"extension events unavailable"})),
        )
            .into_response();
    };
    let receiver = writer.changes().subscribe();
    let ready = futures_util::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().event("ready").data("{}"))
    });
    let changes = futures_util::stream::unfold(receiver, |mut receiver| async move {
        let event = match receiver.recv().await {
            Ok(change) => Event::default().event("change").data(change.to_string()),
            Err(RecvError::Lagged(skipped)) => Event::default()
                .event("resync")
                .data(json!({"skipped": skipped}).to_string()),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(event), receiver))
    });
    Sse::new(ready.chain(changes))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
