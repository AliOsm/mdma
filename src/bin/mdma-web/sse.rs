// Datastar event helpers and the per-tab status stream.
use super::pages::{self, View};
use super::upstream::{Status, Upstream};
use rama::error::BoxError;
use rama::futures::async_stream::stream_fn;
use rama::http::protocols::html::IntoHtml;
use rama::http::service::web::extract::{Query, State};
use rama::http::service::web::response::{IntoResponse, Sse};
use rama::http::sse::datastar::{DatastarEvent, ElementPatchMode, PatchElements, PatchSignals};
use rama::http::sse::server::KeepAlive;
use rama::utils::str::NonEmptyStr;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(super) type Event = Result<DatastarEvent, BoxError>;

fn patch(html: impl IntoHtml) -> Result<PatchElements, BoxError> {
    Ok(PatchElements::new(NonEmptyStr::try_from(
        html.into_string(),
    )?))
}

/// Morphs the rendered element(s) into the DOM by id.
pub(super) fn element(html: impl IntoHtml) -> Event {
    Ok(patch(html)?.try_into_datastar_event()?)
}

pub(super) fn append(selector: &'static str, html: impl IntoHtml) -> Event {
    Ok(patch(html)?
        .with_selector(NonEmptyStr::try_from(selector)?)
        .with_mode(ElementPatchMode::Append)
        .try_into_datastar_event()?)
}

pub(super) fn signals(value: &serde_json::Value) -> Event {
    Ok(PatchSignals::new(value.to_string()).try_into_datastar_event()?)
}

/// The three.js stage's input, as one JSON string signal.
pub(super) fn scene(status: &Status) -> serde_json::Value {
    json!({ "_scene": json!({ "status": status }).to_string() })
}

/// What one status change patches into a page showing `view`.
pub(super) fn status_events(
    status: &Status,
    view: View,
    models_changed: bool,
    pin: &str,
    held: &[String],
) -> Vec<Event> {
    let mut events = vec![signals(&scene(status)), element(pages::status_pill(status))];
    match view {
        View::Memory => events.push(element(pages::models_panel(status))),
        View::Federation => events.push(element(pages::remote_panel(status, pin, held))),
        View::Playground if models_changed => events.push(element(pages::model_select(status))),
        View::Playground => {}
    }
    events
}

#[derive(Debug, Deserialize)]
pub(super) struct StatusQuery {
    #[serde(default)]
    view: View,
}

pub(super) async fn status(
    State(upstream): State<Arc<Upstream>>,
    Query(query): Query<StatusQuery>,
) -> impl IntoResponse {
    let mut rx = upstream.status.clone();
    Sse::new(stream_fn(move |mut yielder| async move {
        let mut names: Option<Vec<String>> = None;
        loop {
            let status = rx.borrow_and_update().clone();
            let current: Vec<String> = status.models.iter().map(|m| m.name.clone()).collect();
            let models_changed = names.as_ref() != Some(&current);
            names = Some(current);
            for event in status_events(
                &status,
                query.view,
                models_changed,
                &upstream.pin,
                &upstream.leases.held(),
            ) {
                yielder.yield_item(event).await;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }))
    .with_keep_alive(KeepAlive::new())
}

#[cfg(test)]
#[path = "sse/tests.rs"]
pub(super) mod tests;
