//! `x.ai/feedback/drafts/*` extension handlers over a session's `FeedbackDraftStore`.

use agent_client_protocol as acp;
use xai_grok_feedback::{DeleteOutcome, FeedbackDraftStore, UpdateOutcome};

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::FeedbackDraftUpdateRequest;

pub const DRAFTS_METHOD_PREFIX: &str = "x.ai/feedback/drafts/";

#[derive(serde::Deserialize)]
struct FeedbackDraftSessionRequest {
    session_id: String,
}

pub(super) async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let store = feedback_store(agent, &requested_session_id(args)?)?;
    answer(args, store).await
}

/// Answers one `x.ai/feedback/drafts/*` request against the given store.
pub async fn answer(args: &acp::ExtRequest, store: FeedbackDraftStore) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/feedback/drafts/list" => list_feedback_drafts(args, store).await,
        "x.ai/feedback/drafts/get" => get_feedback_draft(args, store).await,
        "x.ai/feedback/drafts/delete" => delete_feedback_draft(args, store).await,
        "x.ai/feedback/drafts/update" => update_feedback_draft(args, store).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

/// The `session_id` a drafts request names.
/// Callers pick the store from it before calling `answer`.
pub fn requested_session_id(args: &acp::ExtRequest) -> Result<String, acp::Error> {
    parse_params::<FeedbackDraftSessionRequest>(args).map(|request| request.session_id)
}

#[derive(serde::Deserialize)]
struct FeedbackDraftRequest {
    session_id: String,
    draft_id: xai_grok_feedback::FeedbackDraftId,
}

pub(super) fn feedback_store(
    agent: &MvpAgent,
    session_id: &str,
) -> Result<xai_grok_feedback::FeedbackDraftStore, acp::Error> {
    let session_id = acp::SessionId::new(session_id.to_owned());
    let handle = agent.resident_handle(&session_id).ok_or_else(|| {
        acp::Error::invalid_params().data(format!("session not found: {session_id}"))
    })?;
    Ok(xai_grok_feedback::FeedbackDraftStore::new(
        crate::session::persistence::session_dir(&handle.info),
    ))
}

async fn draft_op<T: Send + 'static>(
    store_op: impl FnOnce() -> xai_grok_feedback::Result<T> + Send + 'static,
    respond: impl FnOnce(T) -> ExtResult,
) -> ExtResult {
    let result = tokio::task::spawn_blocking(store_op)
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    match result {
        Ok(value) => respond(value),
        Err(error) => Err(acp::Error::internal_error().data(error.to_string())),
    }
}

async fn list_feedback_drafts(args: &acp::ExtRequest, store: FeedbackDraftStore) -> ExtResult {
    let _session_id = requested_session_id(args)?;
    draft_op(
        move || store.list(),
        |drafts| super::to_raw_response(&serde_json::json!({ "drafts": drafts })),
    )
    .await
}

async fn get_feedback_draft(args: &acp::ExtRequest, store: FeedbackDraftStore) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let draft_id = request.draft_id;
    draft_op(
        move || store.get(&draft_id),
        |draft| match draft {
            Some(draft) => super::to_raw_response(&serde_json::json!({ "draft": draft })),
            None => Err(acp::Error::invalid_params().data("feedback draft not found")),
        },
    )
    .await
}

async fn update_feedback_draft(args: &acp::ExtRequest, store: FeedbackDraftStore) -> ExtResult {
    let request: FeedbackDraftUpdateRequest = parse_params(args)?;
    draft_op(
        move || store.update_from_input(&request.draft_id, request.input),
        |updated| {
            super::to_raw_response(&serde_json::json!({
                "updated": matches!(updated, UpdateOutcome::Updated),
            }))
        },
    )
    .await
}

async fn delete_feedback_draft(args: &acp::ExtRequest, store: FeedbackDraftStore) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let draft_id = request.draft_id;
    draft_op(
        move || store.delete(&draft_id),
        |deleted| {
            super::to_raw_response(&serde_json::json!({
                "deleted": matches!(deleted, DeleteOutcome::Deleted),
            }))
        },
    )
    .await
}
