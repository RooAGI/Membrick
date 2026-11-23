//! A resident listens before its memory stores are loaded, so clients find it
//! at once and never start a second resident for the same workspace. Requests
//! that arrive while the stores load wait here, then reach the real routes.

use axum::{
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    Json, Router,
};
use hyper::StatusCode;
use serde_json::json;
use tokio::sync::watch;
use tower::ServiceExt;

/// Where a resident is in loading its memory stores.
#[derive(Clone)]
pub(crate) enum ResidentLoad {
    Loading,
    Ready(Router),
    Failed(String),
}

pub(crate) fn resident_load_channel() -> (watch::Sender<ResidentLoad>, watch::Receiver<ResidentLoad>)
{
    watch::channel(ResidentLoad::Loading)
}

/// The router a resident serves from the moment it listens: each request waits
/// until loading ends, then goes to the loaded routes, or gets 503 with the
/// load error.
pub(crate) fn loading_gate_router(load: watch::Receiver<ResidentLoad>) -> Router {
    Router::new().fallback(move |request: Request<Body>| {
        let load = load.clone();
        async move { forward_when_loaded(load, request).await }
    })
}

async fn forward_when_loaded(
    mut load: watch::Receiver<ResidentLoad>,
    request: Request<Body>,
) -> Response {
    let state = match load
        .wait_for(|state| !matches!(state, ResidentLoad::Loading))
        .await
    {
        Ok(state) => state.clone(),
        Err(_) => ResidentLoad::Failed("the resident stopped before its memory loaded".into()),
    };
    match state {
        ResidentLoad::Ready(router) => router
            .oneshot(request)
            .await
            .unwrap_or_else(|never| match never {}),
        ResidentLoad::Failed(message) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "detail": format!("memory store failed to load: {message}") })),
        )
            .into_response(),
        ResidentLoad::Loading => unreachable!("wait_for returns only after loading ends"),
    }
}

/// File-queue requests are taken only after loading ends, so a queue scan never
/// blocks the resident's accept loop while the stores load.
pub(crate) fn resident_load_finished(load: Option<&watch::Receiver<ResidentLoad>>) -> bool {
    load.is_none_or(|load| !matches!(*load.borrow(), ResidentLoad::Loading))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use http_body_util::BodyExt;

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn requests_wait_for_loading_then_reach_the_loaded_routes() {
        let (loaded, load) = resident_load_channel();
        let gate = loading_gate_router(load.clone());
        assert!(!resident_load_finished(Some(&load)));
        let pending = tokio::spawn(
            gate.clone()
                .oneshot(Request::get("/health").body(Body::empty()).unwrap()),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !pending.is_finished(),
            "a request waits while the stores load"
        );

        loaded
            .send(ResidentLoad::Ready(
                Router::new().route("/health", get(|| async { "ok" })),
            ))
            .unwrap();
        let response = pending.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "ok");
        assert!(resident_load_finished(Some(&load)));
        assert!(resident_load_finished(None));
    }

    #[tokio::test]
    async fn a_failed_load_answers_503_with_the_reason() {
        let (loaded, load) = resident_load_channel();
        let gate = loading_gate_router(load);
        loaded
            .send(ResidentLoad::Failed("disk full".into()))
            .unwrap();
        let response = gate
            .oneshot(Request::get("/v1/search").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(body_text(response).await.contains("disk full"));
    }
}
