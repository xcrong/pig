use super::*;
use crate::GrokComConfig;
use xai_grok_test_support::{MockCanAdministerTeam, MockInferenceServer};

const USER_A: &str = "a@acme.test";

fn team_auth() -> GrokAuth {
    GrokAuth {
        oidc_issuer: Some(crate::XAI_OAUTH2_ISSUER.to_owned()),
        email: Some(USER_A.to_owned()),
        principal_type: Some(crate::model::TEAM_PRINCIPAL_TYPE.to_owned()),
        team_id: Some("team-a".to_owned()),
        ..GrokAuth::test_default()
    }
}

async fn denying_server() -> (MockInferenceServer, AuthManager, tempfile::TempDir) {
    let server = MockInferenceServer::start().await.unwrap();
    server.set_user_can_administer_team(MockCanAdministerTeam::Denied);
    let dir = tempfile::tempdir().unwrap();
    let manager =
        AuthManager::new(dir.path(), GrokComConfig::default()).with_proxy_base_url(&server.url());
    manager.hot_swap(team_auth());
    (server, manager, dir)
}

/// Hydration merges the fetched capability into the live credential for the same identity; another identity hears `None`.
#[tokio::test]
async fn hydration_merges_for_same_identity_only() {
    let (_server, manager, _home) = denying_server().await;
    let answer = manager
        .hydrate_can_administer_team(Some(USER_A), Some("team-a"))
        .await;
    assert_eq!(answer, Some(false));
    assert_eq!(manager.current().unwrap().can_administer_team, Some(false));
    manager.hot_swap(GrokAuth {
        key: "key-b".to_owned(),
        email: Some("b@acme.test".into()),
        ..team_auth()
    });
    let answer = manager
        .hydrate_can_administer_team(Some(USER_A), Some("team-a"))
        .await;
    assert_eq!(answer, None);
}
