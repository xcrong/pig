use crate::app::actions::Effect;
use crate::app::app_view::AppView;
use crate::views::setup_wizard::{SetupConfirmRequest, SetupVendorOption};

/// Build wizard options from the shell's builtin vendor table so new
/// snapshots appear without pager changes.
fn builtin_options() -> Vec<SetupVendorOption> {
    xai_grok_shell::util::config::builtin_vendor_options()
        .into_iter()
        .map(|opt| SetupVendorOption {
            id: opt.id.to_string(),
            display_name: opt.display_name.to_string(),
            suggested_env_key: opt.suggested_env_key.to_string(),
        })
        .collect()
}

/// Open the wizard. Detection runs later, once the user picks a vendor
/// or types an env name.
pub(super) fn dispatch_open_setup_wizard(app: &mut AppView) -> Vec<Effect> {
    app.setup_wizard = Some(crate::views::setup_wizard::SetupWizardState::new(
        builtin_options(),
    ));
    app.welcome_menu_index = None;
    vec![]
}

pub(super) fn dispatch_setup_wizard_cancel(app: &mut AppView) -> Vec<Effect> {
    app.setup_wizard = None;
    vec![]
}

/// Confirm emits the async persist; the wizard shows Saving meanwhile.
/// Secrets (if any) were already cleared from the editor on confirm.
pub(super) fn dispatch_setup_wizard_confirm(
    app: &mut AppView,
    req: SetupConfirmRequest,
) -> Vec<Effect> {
    if let Some(wizard) = app.setup_wizard.as_mut() {
        wizard.mark_saving();
    }
    match req {
        SetupConfirmRequest::Vendor {
            vendor_id,
            env_key,
            api_key,
        } => vec![Effect::PersistVendorSetup {
            vendor_id,
            env_key,
            api_key,
        }],
        SetupConfirmRequest::CustomProvider {
            provider_id,
            base_url,
            model_key,
            wire_model,
            api_backend,
            env_key,
            api_key,
        } => vec![Effect::PersistCustomProvider {
            provider_id,
            base_url,
            model_key,
            wire_model,
            api_backend,
            env_key,
            api_key,
        }],
    }
}

/// Async vendor persist result: Done shows the path + restart hint.
/// Never echoes secrets.
pub(super) fn dispatch_vendor_setup_persisted(
    app: &mut AppView,
    vendor_id: String,
    result: Result<String, String>,
) -> Vec<Effect> {
    let Some(wizard) = app.setup_wizard.as_mut() else {
        return vec![];
    };
    match result {
        Ok(path) => {
            wizard.mark_done(path, None);
            // Surface the same restart hint outside the modal so it survives close.
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Info,
                message: format!(
                    "Provider '{vendor_id}' configured. Restart pig to load its models."
                ),
                action: None,
            });
        }
        Err(error) => wizard.mark_failed(error),
    }
    vec![]
}

/// Async custom-provider persist result: Done shows the path plus the
/// `/model <key>` follow-up so the user knows what to run after restart.
pub(super) fn dispatch_custom_provider_persisted(
    app: &mut AppView,
    model_key: String,
    result: Result<String, String>,
) -> Vec<Effect> {
    let Some(wizard) = app.setup_wizard.as_mut() else {
        return vec![];
    };
    match result {
        Ok(path) => {
            wizard.mark_done(path, Some(format!("/model {model_key} after restart")));
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Info,
                message: format!(
                    "Provider model '{model_key}' configured. Restart pig, then run /model {model_key}."
                ),
                action: None,
            });
        }
        Err(error) => wizard.mark_failed(error),
    }
    vec![]
}
