//! `x.ai/rollout/survey` extension handler.

use agent_client_protocol as acp;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::{RolloutSurveyRequest, RolloutSurveyResponse};

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(_agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/rollout/survey" => {
            let req: RolloutSurveyRequest = parse_params(args)?;

            tracing::info!(
                "Rollout survey received for session {}: preferences={:?}, feedback={}",
                req.session_id,
                req.preferences,
                req.feedback,
            );

            to_raw_response(&RolloutSurveyResponse { success: true })
        }
        _ => Err(acp::Error::method_not_found()),
    }
}
