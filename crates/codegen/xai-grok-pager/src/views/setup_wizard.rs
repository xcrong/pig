//! First-run provider setup wizard.
//!
//! Shown on the welcome screen when no credentials are configured
//! (`auth_methods` empty). Lists builtin vendors from the shell's `VENDORS`
//! table so new snapshots appear automatically, detects the suggested
//! `env_key` only after the user picks a vendor (presence + length, value
//! never logged or echoed), and persists via the trusted `config.toml`
//! rewrite path. Custom providers get a step-by-step form for a custom
//! endpoint (provider id, base URL, model, wire id, display name, backend,
//! credential) with a messages-auth branch (gateway Bearer vs Anthropic
//! direct `x-api-key` + `anthropic-version`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::theme::Theme;

/// One builtin vendor offered by the wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupVendorOption {
    pub id: String,
    pub display_name: String,
    pub suggested_env_key: String,
}

/// Credential entry mode on the key step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetupKeyMode {
    /// Use an env var name (recommended, nothing secret on disk).
    #[default]
    UseEnv,
    /// Paste the key directly (stored as `api_key`, plaintext).
    PasteKey,
}

/// Presence of the suggested env var (length only, never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetupEnvPresence {
    pub present: bool,
    pub len: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    ChooseVendor {
        selected: usize,
    },
    EnterKey {
        vendor_index: usize,
    },
    CustomProviderId,
    CustomBaseUrl,
    CustomModelKey,
    CustomWireModel,
    CustomDisplayName,
    CustomBackend {
        selected: usize,
    },
    CustomMessagesAuth {
        selected: usize,
    },
    CustomAnthropicVersion,
    CustomKey,
    CustomConfirm,
    Saving,
    Done {
        config_path: String,
        follow_up: Option<String>,
    },
    Failed {
        error: String,
    },
}

pub struct SetupWizardState {
    pub vendors: Vec<SetupVendorOption>,
    pub step: SetupStep,
    pub key_mode: SetupKeyMode,
    pub input: LineEditor,
    pub detected: SetupEnvPresence,
    pub error: Option<String>,
    pub custom_provider_id: String,
    pub custom_base_url: String,
    pub custom_model_key: String,
    pub custom_wire_model: String,
    pub custom_display_name: String,
    pub custom_backend: usize,
    /// Messages-auth branch: false = gateway (Bearer, default), true =
    /// Anthropic direct (`x-api-key` on the model layer + version header).
    pub custom_messages_direct: bool,
    pub custom_anthropic_version: String,
    /// Stashed credential draft when stepping back from the key step to the
    /// version step (direct branch), restored on the way forward.
    pub custom_cred_draft: String,
    /// Hit-test rects from the last render (dialog + selectable rows).
    /// Populated by `render_setup_wizard`; `None` until the first draw or
    /// when the dialog is too small to render. Read by `handle_mouse` so
    /// hover/click maps use the exact same layout as painting.
    pub hit_areas: Option<SetupWizardHitRects>,
}

/// Hit-test rects for the setup wizard dialog.
#[derive(Debug, Clone, Default)]
pub struct SetupWizardHitRects {
    /// Centered dialog rect (border included).
    pub dialog: Rect,
    /// One rect per selectable list row, in list order.
    /// Empty for non-list steps (input/confirm/hint/saving/done/failed).
    pub rows: Vec<Rect>,
}

/// Persist request emitted on confirm. Exactly one credential is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupConfirmRequest {
    Vendor {
        vendor_id: String,
        env_key: Option<String>,
        api_key: Option<String>,
    },
    CustomProvider {
        provider_id: String,
        base_url: String,
        model_key: String,
        wire_model: String,
        display_name: String,
        api_backend: String,
        /// `Some("x_api_key")` for the messages-direct branch, else `None`.
        auth_scheme: Option<String>,
        /// `anthropic-version` header, non-empty only for messages-direct.
        anthropic_version: String,
        env_key: Option<String>,
        api_key: Option<String>,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SetupWizardOutcome {
    Changed,
    Unchanged,
    Cancelled,
    Confirm(SetupConfirmRequest),
}

/// Backend ids in wizard order, mirroring the shell's `CUSTOM_BACKENDS`.
fn custom_backend_ids() -> Vec<&'static str> {
    xai_grok_shell::util::config::CUSTOM_BACKENDS
        .iter()
        .map(|(id, _)| *id)
        .collect()
}

fn custom_backend_label(index: usize) -> String {
    let backends = xai_grok_shell::util::config::CUSTOM_BACKENDS;
    backends
        .get(index)
        .map(|(id, desc)| format!("{id} — {desc}"))
        .unwrap_or_default()
}

fn valid_table_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `true` when the wizard's selected backend is Anthropic Messages.
fn is_messages_backend(selected: usize) -> bool {
    custom_backend_ids().get(selected).copied() == Some("messages")
}

fn default_anthropic_version() -> String {
    xai_grok_shell::util::config::DEFAULT_ANTHROPIC_VERSION.to_string()
}

impl SetupWizardState {
    pub fn new(vendors: Vec<SetupVendorOption>) -> Self {
        Self {
            vendors,
            step: SetupStep::ChooseVendor { selected: 0 },
            key_mode: SetupKeyMode::UseEnv,
            input: LineEditor::default(),
            detected: SetupEnvPresence::default(),
            error: None,
            custom_provider_id: String::new(),
            custom_base_url: String::new(),
            custom_model_key: String::new(),
            custom_wire_model: String::new(),
            custom_display_name: String::new(),
            custom_backend: 0,
            custom_messages_direct: false,
            custom_anthropic_version: String::new(),
            custom_cred_draft: String::new(),
            hit_areas: None,
        }
    }

    /// Row count on the messages-auth branch step.
    pub fn custom_messages_auth_len() -> usize {
        2
    }

    /// Row count on the choose step: vendors + custom row.
    pub fn choose_len(&self) -> usize {
        self.vendors.len() + 1
    }

    fn vendor_at(&self, index: usize) -> Option<&SetupVendorOption> {
        self.vendors.get(index)
    }

    /// Enter the key step for a builtin vendor, running detection once.
    /// Detection is explicit: called only after the user picked this vendor.
    pub fn enter_vendor(&mut self, index: usize, detect: impl FnOnce(&str) -> SetupEnvPresence) {
        if let Some(vendor) = self.vendor_at(index).cloned() {
            self.detected = detect(&vendor.suggested_env_key);
            // Prefill the env name so Enter confirms; user can still edit it.
            self.input.set_text(&vendor.suggested_env_key);
            self.key_mode = SetupKeyMode::UseEnv;
            self.error = None;
            self.step = SetupStep::EnterKey {
                vendor_index: index,
            };
        }
    }

    fn current_vendor(&self) -> Option<&SetupVendorOption> {
        match self.step {
            SetupStep::EnterKey { vendor_index } => self.vendors.get(vendor_index),
            _ => None,
        }
    }

    /// Re-run detection for the currently typed env name (name-driven, user input).
    fn refresh_detection(&mut self, detect: &impl Fn(&str) -> SetupEnvPresence) {
        if self.key_mode == SetupKeyMode::UseEnv {
            let name = self.input.text().trim().to_string();
            self.detected = if name.is_empty() {
                SetupEnvPresence::default()
            } else {
                detect(&name)
            };
        }
    }

    pub fn handle_key(
        &mut self,
        key: &KeyEvent,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        // Paste via bracketed/clipboard path is handled by the caller through
        // `insert_paste`; here we only handle the keyboard shortcut form.
        if crate::input::key::is_paste_key(key) {
            return match crate::clipboard::system_clipboard_get() {
                Some(text) => self.insert_paste(&text, detect),
                None => SetupWizardOutcome::Unchanged,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !crate::input::key::is_altgr(key.modifiers)
            && matches!(key.code, KeyCode::Char('c' | 'd' | 'q'))
        {
            return SetupWizardOutcome::Cancelled;
        }
        match self.step.clone() {
            SetupStep::ChooseVendor { mut selected } => match key.code {
                KeyCode::Esc => SetupWizardOutcome::Cancelled,
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.step = SetupStep::ChooseVendor { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(self.choose_len().saturating_sub(1));
                    self.step = SetupStep::ChooseVendor { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    if selected < self.vendors.len() {
                        self.enter_vendor(selected, |name| detect(name));
                        SetupWizardOutcome::Changed
                    } else {
                        self.input.set_text(&self.custom_provider_id);
                        self.error = None;
                        self.step = SetupStep::CustomProviderId;
                        SetupWizardOutcome::Changed
                    }
                }
                _ => {
                    if let Some(outcome) = handle_list_nav(key, &mut selected, self.choose_len()) {
                        self.step = SetupStep::ChooseVendor { selected };
                        return outcome;
                    }
                    SetupWizardOutcome::Unchanged
                }
            },
            SetupStep::EnterKey { vendor_index } => match key.code {
                KeyCode::Esc => {
                    self.step = SetupStep::ChooseVendor {
                        selected: vendor_index.min(self.choose_len().saturating_sub(1)),
                    };
                    self.error = None;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Tab if key.modifiers.is_empty() => {
                    self.toggle_key_mode(Some(vendor_index), detect);
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => self.confirm(),
                _ => {
                    let outcome = self.input.handle_key_with_insert_policy(key, |_| true);
                    match outcome {
                        LineEditOutcome::TextChanged | LineEditOutcome::CursorChanged => {
                            self.refresh_detection(detect);
                            self.error = None;
                            SetupWizardOutcome::Changed
                        }
                        LineEditOutcome::HandledNoChange => SetupWizardOutcome::Changed,
                        LineEditOutcome::Unhandled => SetupWizardOutcome::Unchanged,
                    }
                }
            },
            SetupStep::CustomProviderId => match key.code {
                KeyCode::Esc => {
                    self.step = SetupStep::ChooseVendor {
                        selected: self.vendors.len(),
                    };
                    self.error = None;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let value = self.input.text().trim().to_string();
                    if !valid_table_key(&value) {
                        self.error = Some(
                            "Use [A-Za-z0-9_-], max 64.（仅限字母数字、-、_，最长 64）".to_string(),
                        );
                        return SetupWizardOutcome::Changed;
                    }
                    self.custom_provider_id = value;
                    self.input.set_text(&self.custom_base_url);
                    self.error = None;
                    self.step = SetupStep::CustomBaseUrl;
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomBaseUrl => match key.code {
                KeyCode::Esc => {
                    self.input.set_text(&self.custom_provider_id);
                    self.error = None;
                    self.step = SetupStep::CustomProviderId;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let value = self.input.text().trim().to_string();
                    if !(value.starts_with("http://") || value.starts_with("https://")) {
                        self.error =
                            Some("Must start with http(s)://.（须以 http(s):// 开头）".to_string());
                        return SetupWizardOutcome::Changed;
                    }
                    self.custom_base_url = value;
                    self.input.set_text(&self.custom_model_key);
                    self.error = None;
                    self.step = SetupStep::CustomModelKey;
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomModelKey => match key.code {
                KeyCode::Esc => {
                    self.input.set_text(&self.custom_base_url);
                    self.error = None;
                    self.step = SetupStep::CustomBaseUrl;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let value = self.input.text().trim().to_string();
                    if !valid_table_key(&value) {
                        self.error = Some(
                            "Use [A-Za-z0-9_-], max 64.（仅限字母数字、-、_，最长 64）".to_string(),
                        );
                        return SetupWizardOutcome::Changed;
                    }
                    self.custom_model_key = value;
                    self.input.set_text(&self.custom_wire_model);
                    self.error = None;
                    self.step = SetupStep::CustomWireModel;
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomWireModel => match key.code {
                KeyCode::Esc => {
                    self.input.set_text(&self.custom_model_key);
                    self.error = None;
                    self.step = SetupStep::CustomModelKey;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    // Empty means the wire id equals the catalog key.
                    let value = self.input.text().trim().to_string();
                    if !value.is_empty()
                        && (value.contains(char::is_whitespace) || value.len() > 128)
                    {
                        self.error = Some(
                            "Wire id must not contain spaces, max 128.（不能含空白，最长 128）"
                                .to_string(),
                        );
                        return SetupWizardOutcome::Changed;
                    }
                    self.custom_wire_model = value;
                    self.input.set_text(&self.custom_display_name);
                    self.error = None;
                    self.step = SetupStep::CustomDisplayName;
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomDisplayName => match key.code {
                KeyCode::Esc => {
                    self.input.set_text(&self.custom_wire_model);
                    self.error = None;
                    self.step = SetupStep::CustomWireModel;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    // Optional: empty means no `name` is written.
                    self.custom_display_name = self.input.text().trim().to_string();
                    self.error = None;
                    self.step = SetupStep::CustomBackend {
                        selected: self.custom_backend,
                    };
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomBackend { mut selected } => match key.code {
                KeyCode::Esc => {
                    self.input.set_text(&self.custom_display_name);
                    self.error = None;
                    self.step = SetupStep::CustomDisplayName;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.custom_backend = selected;
                    self.step = SetupStep::CustomBackend { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(custom_backend_ids().len().saturating_sub(1));
                    self.custom_backend = selected;
                    self.step = SetupStep::CustomBackend { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    self.custom_backend = selected;
                    if is_messages_backend(selected) {
                        self.error = None;
                        self.step = SetupStep::CustomMessagesAuth {
                            selected: usize::from(self.custom_messages_direct),
                        };
                        SetupWizardOutcome::Changed
                    } else {
                        self.custom_messages_direct = false;
                        self.start_custom_key(detect);
                        SetupWizardOutcome::Changed
                    }
                }
                _ => {
                    let len = custom_backend_ids().len();
                    if let Some(outcome) = handle_list_nav(key, &mut selected, len) {
                        self.custom_backend = selected;
                        self.step = SetupStep::CustomBackend { selected };
                        return outcome;
                    }
                    SetupWizardOutcome::Unchanged
                }
            },
            SetupStep::CustomMessagesAuth { mut selected } => match key.code {
                KeyCode::Esc => {
                    self.error = None;
                    self.step = SetupStep::CustomBackend {
                        selected: self.custom_backend,
                    };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.step = SetupStep::CustomMessagesAuth { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Down => {
                    selected =
                        (selected + 1).min(Self::custom_messages_auth_len().saturating_sub(1));
                    self.step = SetupStep::CustomMessagesAuth { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    if selected == 1 {
                        self.custom_messages_direct = true;
                        let version = if self.custom_anthropic_version.trim().is_empty() {
                            default_anthropic_version()
                        } else {
                            self.custom_anthropic_version.trim().to_string()
                        };
                        self.custom_anthropic_version = version.clone();
                        self.input.set_text(&version);
                        self.error = None;
                        self.step = SetupStep::CustomAnthropicVersion;
                    } else {
                        self.custom_messages_direct = false;
                        self.start_custom_key(detect);
                    }
                    SetupWizardOutcome::Changed
                }
                _ => {
                    let len = Self::custom_messages_auth_len();
                    if let Some(outcome) = handle_list_nav(key, &mut selected, len) {
                        self.step = SetupStep::CustomMessagesAuth { selected };
                        return outcome;
                    }
                    SetupWizardOutcome::Unchanged
                }
            },
            SetupStep::CustomAnthropicVersion => match key.code {
                KeyCode::Esc => {
                    self.error = None;
                    self.step = SetupStep::CustomMessagesAuth { selected: 1 };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let value = self.input.text().trim().to_string();
                    if value.is_empty() {
                        self.error = Some(
                            "Enter anthropic-version, e.g. 2023-06-01.（请输入版本号）".to_string(),
                        );
                        return SetupWizardOutcome::Changed;
                    }
                    if value.contains(char::is_whitespace) || value.len() > 64 {
                        self.error = Some(
                            "Version must not contain spaces, max 64.（不能含空白，最长 64）"
                                .to_string(),
                        );
                        return SetupWizardOutcome::Changed;
                    }
                    self.custom_anthropic_version = value;
                    // Forward to the credential step, restoring any stashed draft.
                    let draft = self.custom_cred_draft.clone();
                    self.input.set_text(&draft);
                    self.custom_cred_draft.clear();
                    self.refresh_detection(detect);
                    self.error = None;
                    self.step = SetupStep::CustomKey;
                    SetupWizardOutcome::Changed
                }
                _ => self.edit_line(key, detect),
            },
            SetupStep::CustomKey => match key.code {
                KeyCode::Esc if self.custom_messages_direct => {
                    // Stash the credential draft; the version step owns `input`.
                    self.custom_cred_draft = self.input.text().to_string();
                    let version = self.custom_anthropic_version.clone();
                    self.input.set_text(&version);
                    self.error = None;
                    self.step = SetupStep::CustomAnthropicVersion;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Esc => {
                    self.step = SetupStep::CustomBackend {
                        selected: self.custom_backend,
                    };
                    self.error = None;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Tab if key.modifiers.is_empty() => {
                    self.toggle_key_mode(None, detect);
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => self.confirm_custom(),
                _ => {
                    let outcome = self.input.handle_key_with_insert_policy(key, |_| true);
                    match outcome {
                        LineEditOutcome::TextChanged | LineEditOutcome::CursorChanged => {
                            self.refresh_detection(detect);
                            self.error = None;
                            SetupWizardOutcome::Changed
                        }
                        LineEditOutcome::HandledNoChange => SetupWizardOutcome::Changed,
                        LineEditOutcome::Unhandled => SetupWizardOutcome::Unchanged,
                    }
                }
            },
            SetupStep::CustomConfirm => match key.code {
                KeyCode::Esc => {
                    // Back to the credential step preserving typed input.
                    let _ = detect;
                    self.step = SetupStep::CustomKey;
                    self.error = None;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => self.confirm_custom(),
                _ => SetupWizardOutcome::Unchanged,
            },
            SetupStep::Saving => SetupWizardOutcome::Unchanged,
            SetupStep::Done { .. } | SetupStep::Failed { .. } => match key.code {
                KeyCode::Esc | KeyCode::Enter if key.modifiers.is_empty() => {
                    SetupWizardOutcome::Cancelled
                }
                _ => SetupWizardOutcome::Unchanged,
            },
        }
    }

    /// Plain text editing for the single-field custom steps (no detection).
    fn edit_line(
        &mut self,
        key: &KeyEvent,
        _detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        let outcome = self.input.handle_key_with_insert_policy(key, |_| true);
        match outcome {
            LineEditOutcome::TextChanged | LineEditOutcome::CursorChanged => {
                self.error = None;
                SetupWizardOutcome::Changed
            }
            LineEditOutcome::HandledNoChange => SetupWizardOutcome::Changed,
            LineEditOutcome::Unhandled => SetupWizardOutcome::Unchanged,
        }
    }

    /// Toggle env-name vs pasted-key mode. `vendor_index` re-prefills the
    /// suggested name for builtin vendors; `None` is the custom flow.
    fn toggle_key_mode(
        &mut self,
        vendor_index: Option<usize>,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) {
        self.key_mode = match self.key_mode {
            SetupKeyMode::UseEnv => {
                self.input.set_text("");
                SetupKeyMode::PasteKey
            }
            SetupKeyMode::PasteKey => {
                let suggested = vendor_index
                    .and_then(|i| self.vendors.get(i))
                    .map(|v| v.suggested_env_key.as_str())
                    .unwrap_or("");
                self.input.set_text(suggested);
                self.refresh_detection(detect);
                SetupKeyMode::UseEnv
            }
        };
        self.error = None;
    }

    /// Enter the custom credential step with a fresh env-name prefill.
    fn start_custom_key(&mut self, detect: &impl Fn(&str) -> SetupEnvPresence) {
        self.key_mode = SetupKeyMode::UseEnv;
        self.input.set_text("");
        self.custom_cred_draft.clear();
        self.refresh_detection(detect);
        self.error = None;
        self.step = SetupStep::CustomKey;
    }

    pub fn insert_paste(
        &mut self,
        text: &str,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        if !matches!(self.step, SetupStep::EnterKey { .. } | SetupStep::CustomKey) {
            return SetupWizardOutcome::Unchanged;
        }
        let outcome = self.input.insert_paste(text);
        match outcome {
            LineEditOutcome::TextChanged | LineEditOutcome::CursorChanged => {
                self.refresh_detection(detect);
                self.error = None;
                SetupWizardOutcome::Changed
            }
            LineEditOutcome::HandledNoChange => SetupWizardOutcome::Changed,
            LineEditOutcome::Unhandled => SetupWizardOutcome::Unchanged,
        }
    }

    /// Read the credential from the key step. Returns `None` with an inline
    /// error instead of confirming when the input is invalid.
    fn read_credential(&mut self) -> Option<(Option<String>, Option<String>)> {
        match self.key_mode {
            SetupKeyMode::UseEnv => {
                let name = self.input.text().trim().to_string();
                if name.is_empty() {
                    self.error = Some("Enter an env var name.（请输入环境变量名）".to_string());
                    return None;
                }
                if name.contains(char::is_whitespace) || name.contains('=') || name.contains('"') {
                    self.error = Some(
                        "Env var name looks invalid.（变量名不能含空白、= 或 \"）".to_string(),
                    );
                    return None;
                }
                Some((Some(name), None))
            }
            SetupKeyMode::PasteKey => {
                let secret = self.input.text().trim().to_string();
                if secret.is_empty() {
                    self.error = Some("Paste an API key.（请粘贴 API key）".to_string());
                    return None;
                }
                // Clear the secret from the in-memory editor the moment we emit
                // it, so it never lingers in the widget or a later render.
                self.input.set_text("");
                Some((None, Some(secret)))
            }
        }
    }

    fn confirm(&mut self) -> SetupWizardOutcome {
        let Some(vendor) = self.current_vendor().cloned() else {
            return SetupWizardOutcome::Unchanged;
        };
        let Some((env_key, api_key)) = self.read_credential() else {
            return SetupWizardOutcome::Changed;
        };
        self.error = None;
        SetupWizardOutcome::Confirm(SetupConfirmRequest::Vendor {
            vendor_id: vendor.id,
            env_key,
            api_key,
        })
    }

    fn confirm_custom(&mut self) -> SetupWizardOutcome {
        if self.custom_provider_id.trim().is_empty()
            || self.custom_base_url.trim().is_empty()
            || self.custom_model_key.trim().is_empty()
        {
            self.error = Some("Missing provider details; go back.（信息缺失，请返回）".to_string());
            return SetupWizardOutcome::Changed;
        }
        // CustomConfirm reviews what was typed; CustomKey collects it.
        if matches!(self.step, SetupStep::CustomKey) {
            self.step = SetupStep::CustomConfirm;
            self.error = None;
            return SetupWizardOutcome::Changed;
        }
        let Some((env_key, api_key)) = self.read_credential() else {
            return SetupWizardOutcome::Changed;
        };
        let backend = custom_backend_ids()
            .get(self.custom_backend)
            .copied()
            .unwrap_or("chat_completions")
            .to_string();
        let direct = self.custom_messages_direct && backend == "messages";
        let anthropic_version = if direct {
            let version = self.custom_anthropic_version.trim().to_string();
            if version.is_empty() {
                self.error =
                    Some("Missing anthropic-version; go back.（缺少版本号，请返回）".to_string());
                return SetupWizardOutcome::Changed;
            }
            version
        } else {
            String::new()
        };
        self.error = None;
        SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
            provider_id: self.custom_provider_id.trim().to_string(),
            base_url: self.custom_base_url.trim().to_string(),
            model_key: self.custom_model_key.trim().to_string(),
            wire_model: self.custom_wire_model.trim().to_string(),
            display_name: self.custom_display_name.trim().to_string(),
            api_backend: backend,
            auth_scheme: direct.then(|| "x_api_key".to_string()),
            anthropic_version,
            env_key,
            api_key,
        })
    }

    pub fn mark_saving(&mut self) {
        self.step = SetupStep::Saving;
        self.error = None;
    }

    pub fn mark_done(&mut self, config_path: String, follow_up: Option<String>) {
        // Defense in depth: the editor is already cleared on confirm, but a
        // pasted key must never survive into the Done screen either.
        self.input.set_text("");
        self.step = SetupStep::Done {
            config_path,
            follow_up,
        };
        self.error = None;
    }

    pub fn mark_failed(&mut self, error: String) {
        self.input.set_text("");
        self.step = SetupStep::Failed { error };
    }

    #[cfg(test)]
    pub fn set_input(&mut self, text: &str) {
        self.input.set_text(text);
    }
}

fn handle_list_nav(key: &KeyEvent, selected: &mut usize, len: usize) -> Option<SetupWizardOutcome> {
    match key.code {
        KeyCode::Down => {
            *selected = (*selected + 1).min(len.saturating_sub(1));
            Some(SetupWizardOutcome::Changed)
        }
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            Some(SetupWizardOutcome::Changed)
        }
        _ => None,
    }
}

/// Masked input display for pasted secrets: bullets, never the value.
fn masked_input(text: &str) -> String {
    "•".repeat(text.chars().count().min(64))
}

const DIALOG_MIN_WIDTH: u16 = 58;
const DIALOG_HEIGHT: u16 = 16;
const INNER_PAD: u16 = 4;
/// Rows from the dialog top to the first selectable list row:
/// title (1) + blank (1) + subtitle (1) + 1-based offset.
const LIST_START_OFFSET: u16 = 4;

fn dialog_width_for(area_width: u16) -> u16 {
    DIALOG_MIN_WIDTH.min(area_width.saturating_sub(4)).max(20)
}

/// Centered dialog rect, shared by render and hit-testing so hover never
/// drifts from painting. `None` when the terminal is too small for the
/// dialog (the `[Esc] to close` fallback). Must stay in sync with
/// `render_setup_wizard`'s early-return guard.
fn setup_dialog_rect(area: Rect) -> Option<Rect> {
    if area.height < DIALOG_HEIGHT || area.width < 30 {
        return None;
    }
    let width = dialog_width_for(area.width);
    let [_, dialog_h, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, dialog, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(DIALOG_HEIGHT),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(dialog_h);
    Some(dialog)
}

/// One hit rect per selectable row, using the same `inner_x`/`inner_w` as
/// painting (`dialog.x + 2`, `dialog.width - INNER_PAD`).
fn setup_list_rows(dialog: Rect, count: usize) -> Vec<Rect> {
    let inner_x = dialog.x + 2;
    let inner_w = dialog.width.saturating_sub(INNER_PAD);
    let start_y = dialog.y + LIST_START_OFFSET;
    (0..count)
        .map(|i| Rect::new(inner_x, start_y + i as u16, inner_w, 1))
        .collect()
}

/// Full hit layout for the current step. `rows` is empty for non-list
/// steps (the dialog rect alone still swallows clicks). Returns `None`
/// when the dialog itself cannot render.
fn compute_setup_layout(area: Rect, state: &SetupWizardState) -> Option<SetupWizardHitRects> {
    let dialog = setup_dialog_rect(area)?;
    let count = match &state.step {
        SetupStep::ChooseVendor { .. } => state.choose_len(),
        SetupStep::CustomBackend { .. } => custom_backend_ids().len(),
        SetupStep::CustomMessagesAuth { .. } => SetupWizardState::custom_messages_auth_len(),
        _ => 0,
    };
    Some(SetupWizardHitRects {
        dialog,
        rows: setup_list_rows(dialog, count),
    })
}

impl SetupWizardState {
    /// Clear stale hit rects (e.g. on resize before the next render).
    pub fn clear_hit_areas(&mut self) {
        self.hit_areas = None;
    }

    fn list_selection(&self) -> Option<(usize, usize)> {
        match &self.step {
            SetupStep::ChooseVendor { selected } => Some((*selected, self.choose_len())),
            SetupStep::CustomBackend { selected } => Some((*selected, custom_backend_ids().len())),
            SetupStep::CustomMessagesAuth { selected } => {
                Some((*selected, Self::custom_messages_auth_len()))
            }
            _ => None,
        }
    }

    fn set_list_selection(&mut self, index: usize) {
        match &mut self.step {
            SetupStep::ChooseVendor { selected } => *selected = index,
            SetupStep::CustomBackend { selected } => {
                *selected = index;
                self.custom_backend = index;
            }
            SetupStep::CustomMessagesAuth { selected } => *selected = index,
            _ => {}
        }
    }

    /// Activate the given list row as if Enter was pressed on it.
    fn activate_list_row(
        &mut self,
        index: usize,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        match self.step.clone() {
            SetupStep::ChooseVendor { .. } => {
                if index < self.vendors.len() {
                    self.enter_vendor(index, |name| detect(name));
                    SetupWizardOutcome::Changed
                } else {
                    self.input.set_text(&self.custom_provider_id);
                    self.error = None;
                    self.step = SetupStep::CustomProviderId;
                    SetupWizardOutcome::Changed
                }
            }
            SetupStep::CustomBackend { .. } => {
                self.custom_backend = index;
                if is_messages_backend(index) {
                    self.step = SetupStep::CustomMessagesAuth {
                        selected: usize::from(self.custom_messages_direct),
                    };
                } else {
                    self.custom_messages_direct = false;
                    self.start_custom_key(detect);
                }
                SetupWizardOutcome::Changed
            }
            SetupStep::CustomMessagesAuth { .. } => {
                if index == 1 {
                    self.custom_messages_direct = true;
                    let version = if self.custom_anthropic_version.trim().is_empty() {
                        default_anthropic_version()
                    } else {
                        self.custom_anthropic_version.trim().to_string()
                    };
                    self.custom_anthropic_version = version.clone();
                    self.input.set_text(&version);
                    self.step = SetupStep::CustomAnthropicVersion;
                } else {
                    self.custom_messages_direct = false;
                    self.start_custom_key(detect);
                }
                SetupWizardOutcome::Changed
            }
            _ => SetupWizardOutcome::Unchanged,
        }
    }

    fn hit_row_index(&self, column: u16, row: u16) -> Option<usize> {
        let hits = self.hit_areas.as_ref()?;
        hits.rows.iter().position(|r| {
            column >= r.x
                && column < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// Mouse handling for the wizard. Mirrors the keyboard list nav:
    /// hover moves selection, click selects, click on the selected row
    /// confirms. Clicks anywhere (inside or outside the dialog) are
    /// swallowed so they never reach the welcome menu underneath.
    /// Returns `Unchanged` when nothing visibly changed, still consumed
    /// by the caller via early-return.
    pub fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        match kind {
            MouseEventKind::Moved => {
                let Some((selected, _)) = self.list_selection() else {
                    return SetupWizardOutcome::Unchanged;
                };
                // No layout yet (small terminal / pre-first-render): no hover.
                if self.hit_areas.is_none() {
                    return SetupWizardOutcome::Unchanged;
                }
                match self.hit_row_index(column, row) {
                    Some(i) if i != selected => {
                        self.set_list_selection(i);
                        SetupWizardOutcome::Changed
                    }
                    _ => SetupWizardOutcome::Unchanged,
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((selected, _)) = self.list_selection() else {
                    // Non-list steps: swallow the click (no pass-through).
                    return SetupWizardOutcome::Unchanged;
                };
                // No layout yet: swallow without acting (never panic).
                if self.hit_areas.is_none() {
                    return SetupWizardOutcome::Unchanged;
                }
                match self.hit_row_index(column, row) {
                    Some(i) if i == selected => self.activate_list_row(i, detect),
                    Some(i) => {
                        self.set_list_selection(i);
                        SetupWizardOutcome::Changed
                    }
                    // Inside dialog but off the rows, or outside the dialog:
                    // consume to block the welcome menu underneath.
                    None => SetupWizardOutcome::Unchanged,
                }
            }
            // Short lists need no scrolling; swallow so the welcome
            // underneath never sees the wheel.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => SetupWizardOutcome::Unchanged,
            _ => SetupWizardOutcome::Unchanged,
        }
    }
}

/// Render the wizard centered over the welcome screen.
/// Stores hit rects on `state` for `handle_mouse`; callers must pass
/// `&mut` (mirrors `ImportClaudeModalState::content_area`).
pub fn render_setup_wizard(area: Rect, buf: &mut Buffer, state: &mut SetupWizardState) {
    let theme = Theme::current();
    if area.height < DIALOG_HEIGHT || area.width < 30 {
        // No dialog to hit-test; drop stale rects so mouse handlers
        // cannot act on positions from a previous (larger) render.
        state.hit_areas = None;
        if area.height >= 1 && area.width >= 16 {
            let hint = Line::from(Span::styled(
                "[Esc] to close",
                Style::default().fg(theme.gray_dim),
            ));
            hint.render(Rect::new(area.x, area.y, area.width.min(16), 1), buf);
        }
        return;
    }
    let Some(layout) = compute_setup_layout(area, state) else {
        state.hit_areas = None;
        return;
    };
    state.hit_areas = Some(layout.clone());
    let dialog = layout.dialog;

    let bg = Style::default().bg(theme.bg_dark);
    for y in dialog.y..dialog.y + dialog.height {
        for x in dialog.x..dialog.x + dialog.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(bg);
            }
        }
    }
    let border = Style::default().fg(theme.accent_user).bg(theme.bg_dark);
    for x in dialog.x + 1..dialog.x + dialog.width - 1 {
        if let Some(cell) = buf.cell_mut((x, dialog.y)) {
            cell.set_char('─');
            cell.set_style(border);
        }
        let bottom = dialog.y + dialog.height - 1;
        if let Some(cell) = buf.cell_mut((x, bottom)) {
            cell.set_char('─');
            cell.set_style(border);
        }
    }
    for y in dialog.y + 1..dialog.y + dialog.height - 1 {
        if let Some(cell) = buf.cell_mut((dialog.x, y)) {
            cell.set_char('│');
            cell.set_style(border);
        }
        if let Some(cell) = buf.cell_mut((dialog.x + dialog.width - 1, y)) {
            cell.set_char('│');
            cell.set_style(border);
        }
    }
    for (x, y, ch) in [
        (dialog.x, dialog.y, '╭'),
        (dialog.x + dialog.width - 1, dialog.y, '╮'),
        (dialog.x, dialog.y + dialog.height - 1, '╰'),
        (
            dialog.x + dialog.width - 1,
            dialog.y + dialog.height - 1,
            '╯',
        ),
    ] {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_style(border);
        }
    }

    let inner_x = dialog.x + 2;
    let inner_w = dialog.width.saturating_sub(INNER_PAD);
    let mut row = dialog.y + 1;
    paint_line(
        buf,
        inner_x,
        &mut row,
        inner_w,
        Line::from(Span::styled(
            "Setup provider",
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )),
    );
    paint_line(buf, inner_x, &mut row, inner_w, Line::from(Span::raw("")));

    match &state.step {
        SetupStep::ChooseVendor { selected } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Choose a provider (Builtin vendors):",
                    Style::default().fg(theme.gray_bright),
                )),
            );
            for (i, vendor) in state.vendors.iter().enumerate() {
                let focused = *selected == i;
                paint_list_row(
                    buf,
                    Rect::new(inner_x, row, inner_w, 1),
                    focused,
                    &theme,
                    &format!("{} ({})", vendor.display_name, vendor.id),
                );
                row += 1;
            }
            paint_list_row(
                buf,
                Rect::new(inner_x, row, inner_w, 1),
                *selected == state.vendors.len(),
                &theme,
                "Custom provider…",
            );
            row += 2;
            paint_hints(buf, Rect::new(inner_x, row, inner_w, 1), &theme);
        }
        SetupStep::EnterKey { vendor_index } => {
            let vendor = state.vendors.get(*vendor_index);
            let title = vendor
                .map(|v| format!("{} ({})", v.display_name, v.id))
                .unwrap_or_default();
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(vec![
                    Span::styled("Vendor: ", Style::default().fg(theme.gray)),
                    Span::styled(title, Style::default().fg(theme.text_primary)),
                ]),
            );
            let mode_label = match state.key_mode {
                SetupKeyMode::UseEnv => "Mode: env var (recommended)  [Tab] paste key instead",
                SetupKeyMode::PasteKey => "Mode: paste key  [Tab] use env var instead",
            };
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(mode_label, Style::default().fg(theme.gray))),
            );
            match state.key_mode {
                SetupKeyMode::UseEnv => {
                    let status = match (state.detected.present, state.detected.len) {
                        (true, Some(len)) => {
                            format!("found in environment ({} chars, hidden)", len)
                        }
                        (true, None) => "found in environment".to_string(),
                        (false, _) => "not found in environment".to_string(),
                    };
                    let status_fg = if state.detected.present {
                        theme.accent_success
                    } else {
                        theme.warning
                    };
                    paint_line(
                        buf,
                        inner_x,
                        &mut row,
                        inner_w,
                        Line::from(vec![
                            Span::styled("Status: ", Style::default().fg(theme.gray)),
                            Span::styled(status, Style::default().fg(status_fg)),
                        ]),
                    );
                    paint_input_row(
                        buf,
                        Rect::new(inner_x, row, inner_w, 1),
                        &theme,
                        "Env var: ",
                        state.input.text(),
                        &state.input,
                        inner_w,
                    );
                    row += 1;
                    paint_line(
                        buf,
                        inner_x,
                        &mut row,
                        inner_w,
                        Line::from(Span::styled(
                            "Enter = save env_key · export the var before use",
                            Style::default().fg(theme.gray_dim),
                        )),
                    );
                }
                SetupKeyMode::PasteKey => {
                    paint_line(
                        buf,
                        inner_x,
                        &mut row,
                        inner_w,
                        Line::from(Span::styled(
                            "Paste the API key (stored as api_key, plaintext):",
                            Style::default().fg(theme.warning),
                        )),
                    );
                    let masked = masked_input(state.input.text());
                    paint_input_row(
                        buf,
                        Rect::new(inner_x, row, inner_w, 1),
                        &theme,
                        "Key: ",
                        &masked,
                        &state.input,
                        inner_w,
                    );
                    row += 1;
                }
            }
            row += 1;
            if let Some(err) = state.error.as_deref() {
                paint_line(
                    buf,
                    inner_x,
                    &mut row,
                    inner_w,
                    Line::from(Span::styled(err, Style::default().fg(theme.accent_error))),
                );
            } else {
                paint_line(buf, inner_x, &mut row, inner_w, Line::from(Span::raw("")));
            }
            paint_hints(buf, Rect::new(inner_x, row, inner_w, 1), &theme);
        }
        SetupStep::CustomProviderId => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Step 1/7 — Provider id",
                "Short name for [model_providers.<id>]:",
                "Provider id: ",
            );
        }
        SetupStep::CustomBaseUrl => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Step 2/7 — Base URL",
                "Endpoint URL (Messages backends usually end with /v1):",
                "Base URL: ",
            );
        }
        SetupStep::CustomModelKey => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Step 3/7 — Model",
                "Catalog key for [model.<key>] and /model:",
                "Model: ",
            );
        }
        SetupStep::CustomWireModel => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Step 4/7 — Wire model id (optional)",
                "Id sent to the API. Empty = same as key:",
                "Wire id: ",
            );
        }
        SetupStep::CustomDisplayName => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Step 5/7 — Display name (optional)",
                "Picker label for [model.<key>]. Empty = key:",
                "Name: ",
            );
        }
        SetupStep::CustomBackend { selected } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Step 6/7 — API backend:",
                    Style::default().fg(theme.gray_bright),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Unsure? Keep chat_completions. responses = OpenAI Responses API.",
                    Style::default().fg(theme.gray),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "messages = Anthropic Messages (gateway, or Anthropic direct next).",
                    Style::default().fg(theme.gray),
                )),
            );
            for i in 0..custom_backend_ids().len() {
                paint_list_row(
                    buf,
                    Rect::new(inner_x, row, inner_w, 1),
                    *selected == i,
                    &theme,
                    &custom_backend_label(i),
                );
                row += 1;
            }
            row += 1;
            paint_hints(buf, Rect::new(inner_x, row, inner_w, 1), &theme);
        }
        SetupStep::CustomMessagesAuth { selected } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Messages auth — gateway or direct?",
                    Style::default().fg(theme.gray_bright),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Gateway sends Bearer; Anthropic direct sends x-api-key.",
                    Style::default().fg(theme.gray),
                )),
            );
            for (i, label) in ["Gateway (Bearer, default)", "Anthropic direct (x-api-key)"]
                .iter()
                .enumerate()
            {
                paint_list_row(
                    buf,
                    Rect::new(inner_x, row, inner_w, 1),
                    *selected == i,
                    &theme,
                    label,
                );
                row += 1;
            }
            row += 1;
            paint_hints(buf, Rect::new(inner_x, row, inner_w, 1), &theme);
        }
        SetupStep::CustomAnthropicVersion => {
            render_custom_field(
                buf,
                inner_x,
                &mut row,
                inner_w,
                &theme,
                state,
                "Anthropic API version",
                "Sent as the anthropic-version header:",
                "Version: ",
            );
        }
        SetupStep::CustomKey => {
            render_custom_key(buf, inner_x, &mut row, inner_w, &theme, state);
        }
        SetupStep::CustomConfirm => {
            let wire = if state.custom_wire_model.trim().is_empty() {
                format!("{} (same as key)", state.custom_model_key.trim())
            } else {
                state.custom_wire_model.trim().to_string()
            };
            let backend = custom_backend_ids()
                .get(state.custom_backend)
                .copied()
                .unwrap_or("chat_completions");
            let direct = state.custom_messages_direct && backend == "messages";
            let auth_line = if direct {
                format!(
                    "auth: x-api-key + anthropic-version {}",
                    state.custom_anthropic_version.trim()
                )
            } else {
                "auth: Bearer (Authorization)".to_string()
            };
            let cred_line = match state.key_mode {
                SetupKeyMode::UseEnv => {
                    let name = state.input.text().trim().to_string();
                    let status = match (state.detected.present, state.detected.len) {
                        (true, Some(len)) => format!("found ({} chars, hidden)", len),
                        (true, None) => "found".to_string(),
                        (false, _) => format!("NOT FOUND — export {name}=... and restart"),
                    };
                    format!("env_key = \"{name}\" ({status})")
                }
                SetupKeyMode::PasteKey => "api_key = ••• (hidden)".to_string(),
            };
            let cred_fg = match state.key_mode {
                SetupKeyMode::UseEnv if !state.detected.present => theme.warning,
                _ => theme.gray,
            };
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Confirm custom provider:",
                    Style::default().fg(theme.gray_bright),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    format!(
                        "[model_providers.{}] {}",
                        state.custom_provider_id.trim(),
                        state.custom_base_url.trim()
                    ),
                    Style::default().fg(theme.text_primary),
                )),
            );
            let mut model_line = format!(
                "[model.{}] {} via {}",
                state.custom_model_key.trim(),
                wire,
                backend
            );
            if !state.custom_display_name.trim().is_empty() {
                model_line.push_str(&format!(" \"{}\"", state.custom_display_name.trim()));
            }
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    model_line,
                    Style::default().fg(theme.text_primary),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    auth_line,
                    Style::default().fg(theme.gray_bright),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(cred_line, Style::default().fg(cred_fg))),
            );
            if backend == "responses" {
                paint_line(
                    buf,
                    inner_x,
                    &mut row,
                    inner_w,
                    Line::from(Span::styled(
                        "reasoning_summary defaults to concise; use none if rejected (see guide).",
                        Style::default().fg(theme.gray),
                    )),
                );
            }
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Advanced (session_header/context_window/...): see 11-custom-models.md.",
                    Style::default().fg(theme.gray_dim),
                )),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(vec![
                    Span::styled("enter", bold_accent(&theme)),
                    Span::styled(" = save   ", Style::default().fg(theme.gray)),
                    Span::styled("esc", bold_accent(&theme)),
                    Span::styled(" = back", Style::default().fg(theme.gray)),
                ]),
            );
            if let Some(err) = state.error.as_deref() {
                paint_line(
                    buf,
                    inner_x,
                    &mut row,
                    inner_w,
                    Line::from(Span::styled(err, Style::default().fg(theme.accent_error))),
                );
            }
        }
        SetupStep::Saving => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Saving to config.toml…",
                    Style::default().fg(theme.gray_bright),
                )),
            );
        }
        SetupStep::Done {
            config_path,
            follow_up,
        } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Saved. Restart pig to load models.",
                    Style::default().fg(theme.accent_success),
                )),
            );
            let short = ellipsize_middle(config_path, inner_w as usize);
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(vec![
                    Span::styled("Config: ", Style::default().fg(theme.gray)),
                    Span::styled(short, Style::default().fg(theme.text_primary)),
                ]),
            );
            if let Some(hint) = follow_up {
                let short_hint = ellipsize_middle(hint, inner_w as usize);
                paint_line(
                    buf,
                    inner_x,
                    &mut row,
                    inner_w,
                    Line::from(Span::styled(
                        short_hint,
                        Style::default().fg(theme.gray_bright),
                    )),
                );
            }
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(vec![
                    Span::styled("enter/esc", bold_accent(&theme)),
                    Span::styled(" = close", Style::default().fg(theme.gray)),
                ]),
            );
        }
        SetupStep::Failed { error } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Save failed.",
                    Style::default().fg(theme.accent_error),
                )),
            );
            let short = ellipsize_middle(error, inner_w as usize);
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(short, Style::default().fg(theme.gray_bright))),
            );
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(vec![
                    Span::styled("enter/esc", bold_accent(&theme)),
                    Span::styled(" = close", Style::default().fg(theme.gray)),
                ]),
            );
        }
    }
}

fn paint_line(buf: &mut Buffer, x: u16, row: &mut u16, width: u16, line: Line<'_>) {
    line.render(Rect::new(x, *row, width, 1), buf);
    *row += 1;
}

fn bold_accent(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD)
}

fn paint_list_row(buf: &mut Buffer, rect: Rect, focused: bool, theme: &Theme, label: &str) {
    if focused {
        for x in rect.x..rect.x + rect.width {
            if let Some(cell) = buf.cell_mut((x, rect.y)) {
                cell.set_style(Style::default().bg(theme.bg_highlight));
            }
        }
    }
    let prefix = if focused { "> " } else { "  " };
    let fg = if focused {
        theme.text_primary
    } else {
        theme.gray
    };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.accent_user)),
        Span::styled(label, Style::default().fg(fg)),
    ])
    .render(rect, buf);
}

fn paint_input_row(
    buf: &mut Buffer,
    rect: Rect,
    theme: &Theme,
    prefix: &str,
    visible: &str,
    editor: &LineEditor,
    inner_w: u16,
) {
    let prefix_w = prefix.width() as u16;
    let input_w = inner_w.saturating_sub(prefix_w).max(1) as usize;
    let viewport = editor.viewport(input_w);
    let text = visible.get(viewport.visible_byte_range).unwrap_or("");
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.gray_bright)),
        Span::styled(text, Style::default().fg(theme.text_primary)),
    ])
    .render(rect, buf);
    let cursor_x = rect.x + prefix_w + viewport.cursor_display_column as u16;
    if let Some(cell) = buf.cell_mut((cursor_x, rect.y)) {
        cell.set_style(theme.block_cursor_over(theme.bg_dark));
    }
}

/// One-line text step for the custom flow: title + hint + input + error + hints.
#[allow(clippy::too_many_arguments)]
fn render_custom_field(
    buf: &mut Buffer,
    inner_x: u16,
    row: &mut u16,
    inner_w: u16,
    theme: &Theme,
    state: &SetupWizardState,
    title: &str,
    hint: &str,
    prefix: &str,
) {
    paint_line(
        buf,
        inner_x,
        row,
        inner_w,
        Line::from(Span::styled(title, Style::default().fg(theme.gray_bright))),
    );
    paint_line(
        buf,
        inner_x,
        row,
        inner_w,
        Line::from(Span::styled(hint, Style::default().fg(theme.gray))),
    );
    paint_input_row(
        buf,
        Rect::new(inner_x, *row, inner_w, 1),
        theme,
        prefix,
        state.input.text(),
        &state.input,
        inner_w,
    );
    *row += 1;
    if let Some(err) = state.error.as_deref() {
        paint_line(
            buf,
            inner_x,
            row,
            inner_w,
            Line::from(Span::styled(err, Style::default().fg(theme.accent_error))),
        );
    } else {
        paint_line(buf, inner_x, row, inner_w, Line::from(Span::raw("")));
    }
    paint_hints(buf, Rect::new(inner_x, *row, inner_w, 1), theme);
}

/// Credential step for the custom flow (step 6 header + shared key UI).
fn render_custom_key(
    buf: &mut Buffer,
    inner_x: u16,
    row: &mut u16,
    inner_w: u16,
    theme: &Theme,
    state: &SetupWizardState,
) {
    let mode_label = match state.key_mode {
        SetupKeyMode::UseEnv => "Step 7/7 — Credential: env var (recommended)  [Tab] paste instead",
        SetupKeyMode::PasteKey => "Step 7/7 — Credential: paste key  [Tab] use env var instead",
    };
    paint_line(
        buf,
        inner_x,
        row,
        inner_w,
        Line::from(Span::styled(mode_label, Style::default().fg(theme.gray))),
    );
    match state.key_mode {
        SetupKeyMode::UseEnv => {
            let status = match (state.detected.present, state.detected.len) {
                (true, Some(len)) => format!("found in environment ({} chars, hidden)", len),
                (true, None) => "found in environment".to_string(),
                (false, _) => "not found in environment".to_string(),
            };
            let status_fg = if state.detected.present {
                theme.accent_success
            } else {
                theme.warning
            };
            paint_line(
                buf,
                inner_x,
                row,
                inner_w,
                Line::from(vec![
                    Span::styled("Status: ", Style::default().fg(theme.gray)),
                    Span::styled(status, Style::default().fg(status_fg)),
                ]),
            );
            paint_input_row(
                buf,
                Rect::new(inner_x, *row, inner_w, 1),
                theme,
                "Env var: ",
                state.input.text(),
                &state.input,
                inner_w,
            );
            *row += 1;
        }
        SetupKeyMode::PasteKey => {
            let masked = masked_input(state.input.text());
            paint_input_row(
                buf,
                Rect::new(inner_x, *row, inner_w, 1),
                theme,
                "Key: ",
                &masked,
                &state.input,
                inner_w,
            );
            *row += 1;
        }
    }
    if let Some(err) = state.error.as_deref() {
        paint_line(
            buf,
            inner_x,
            row,
            inner_w,
            Line::from(Span::styled(err, Style::default().fg(theme.accent_error))),
        );
    } else {
        paint_line(buf, inner_x, row, inner_w, Line::from(Span::raw("")));
    }
    // Next stop is the review screen; Esc returns to the backend list.
    Line::from(vec![
        Span::styled("enter", bold_accent(theme)),
        Span::styled(" = review   ", Style::default().fg(theme.gray)),
        Span::styled("esc", bold_accent(theme)),
        Span::styled(" = back", Style::default().fg(theme.gray)),
    ])
    .render(Rect::new(inner_x, *row, inner_w, 1), buf);
}

fn paint_hints(buf: &mut Buffer, rect: Rect, theme: &Theme) {
    Line::from(vec![
        Span::styled("enter", bold_accent(theme)),
        Span::styled(" = ok   ", Style::default().fg(theme.gray)),
        Span::styled("esc", bold_accent(theme)),
        Span::styled(" = back", Style::default().fg(theme.gray)),
    ])
    .render(rect, buf);
}

fn ellipsize_middle(text: &str, max: usize) -> String {
    if max == 0 || text.len() <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let keep = max - 3;
    let front = keep / 2 + keep % 2;
    let back = keep / 2;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    format!(
        "{}…{}",
        chars.iter().take(front).collect::<String>(),
        chars.iter().skip(chars.len() - back).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<SetupVendorOption> {
        vec![
            SetupVendorOption {
                id: "opencode".to_string(),
                display_name: "OpenCode Zen".to_string(),
                suggested_env_key: "OPENCODE_API_KEY".to_string(),
            },
            SetupVendorOption {
                id: "opencode-go".to_string(),
                display_name: "OpenCode Go".to_string(),
                suggested_env_key: "OPENCODE_API_KEY".to_string(),
            },
        ]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn choose_enter_runs_detection_once() {
        let mut state = SetupWizardState::new(options());
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence {
            present: true,
            len: Some(32),
        });
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::EnterKey { vendor_index: 0 }
        ));
        assert_eq!(state.input.text(), "OPENCODE_API_KEY");
        assert!(state.detected.present);
        // Value never stored: only presence + length.
        assert_eq!(state.detected.len, Some(32));
    }

    #[test]
    fn env_confirm_emits_env_key() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        state.set_input("MY_KEY");
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::Vendor {
                vendor_id,
                env_key,
                api_key,
            }) => {
                assert_eq!(vendor_id, "opencode");
                assert_eq!(env_key.as_deref(), Some("MY_KEY"));
                assert!(api_key.is_none());
            }
            other => panic!("expected confirm, got {other:?}"),
        }
    }

    #[test]
    fn paste_mode_masks_and_clears_secret() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        state.handle_key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()), &|_| {
            SetupEnvPresence::default()
        });
        assert_eq!(state.key_mode, SetupKeyMode::PasteKey);
        state.set_input("sk-secret");
        assert_eq!(masked_input(state.input.text()), "•••••••••");
        assert!(!masked_input(state.input.text()).contains("sk-secret"));
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::Vendor { api_key, .. }) => {
                assert_eq!(api_key.as_deref(), Some("sk-secret"));
            }
            other => panic!("expected confirm, got {other:?}"),
        }
        assert_eq!(
            state.input.text(),
            "",
            "secret cleared from editor on confirm"
        );
    }

    #[test]
    fn custom_row_goes_to_provider_id() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Down), &|_| SetupEnvPresence::default());
        state.handle_key(&key(KeyCode::Down), &|_| SetupEnvPresence::default());
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomProviderId));
    }

    fn enter_custom_openai(state: &mut SetupWizardState) {
        let no_detect = |_: &str| SetupEnvPresence::default();
        // ChooseVendor (custom row) jumps straight to the endpoint flow.
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomProviderId));
    }

    fn type_text(state: &mut SetupWizardState, text: &str) {
        state.set_input(text);
    }

    /// Drive the wizard to the messages-auth branch step.
    fn enter_custom_messages(
        state: &mut SetupWizardState,
        no_detect: &impl Fn(&str) -> SetupEnvPresence,
    ) {
        enter_custom_openai(state);
        type_text(state, "anthropic");
        state.handle_key(&key(KeyCode::Enter), no_detect);
        type_text(state, "https://api.anthropic.com");
        state.handle_key(&key(KeyCode::Enter), no_detect);
        type_text(state, "claude");
        state.handle_key(&key(KeyCode::Enter), no_detect);
        // Wire empty (= key), display empty (= key).
        state.handle_key(&key(KeyCode::Enter), no_detect);
        state.handle_key(&key(KeyCode::Enter), no_detect);
        assert!(matches!(state.step, SetupStep::CustomBackend { .. }));
        state.handle_key(&key(KeyCode::Down), no_detect);
        state.handle_key(&key(KeyCode::Down), no_detect);
        assert_eq!(state.custom_backend, 2);
        state.handle_key(&key(KeyCode::Enter), no_detect);
        assert!(matches!(state.step, SetupStep::CustomMessagesAuth { .. }));
    }

    #[test]
    fn custom_openai_flow_confirms_provider() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_openai(&mut state);
        // Provider id -> base URL -> model key -> wire (empty = key)
        // -> display name (empty = key) -> backend.
        type_text(&mut state, "acme");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomBaseUrl));
        type_text(&mut state, "https://api.acme.example/v1");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomModelKey));
        type_text(&mut state, "acme-model");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomWireModel));
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomDisplayName));
        type_text(&mut state, "Acme Model");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomBackend { .. }));
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        // Credential -> review -> confirm.
        type_text(&mut state, "ACME_API_KEY");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomConfirm));
        // Review screen shows the shapes; confirm emits without echoing values.
        let outcome = state.handle_key(&key(KeyCode::Enter), &no_detect);
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
                provider_id,
                base_url,
                model_key,
                wire_model,
                display_name,
                api_backend,
                auth_scheme,
                anthropic_version,
                env_key,
                api_key,
            }) => {
                assert_eq!(provider_id, "acme");
                assert_eq!(base_url, "https://api.acme.example/v1");
                assert_eq!(model_key, "acme-model");
                assert_eq!(wire_model, "", "empty wire defaults shell-side");
                assert_eq!(display_name, "Acme Model");
                assert_eq!(api_backend, "chat_completions");
                assert!(auth_scheme.is_none(), "gateway keeps Bearer");
                assert_eq!(anthropic_version, "");
                assert_eq!(env_key.as_deref(), Some("ACME_API_KEY"));
                assert!(api_key.is_none());
            }
            other => panic!("expected custom confirm, got {other:?}"),
        }
    }

    #[test]
    fn custom_validation_blocks_bad_input() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_openai(&mut state);
        type_text(&mut state, "has space");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomProviderId));
        assert!(state.error.is_some());
        type_text(&mut state, "acme");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "ftp://example.com");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomBaseUrl));
        assert!(state.error.is_some());
        type_text(&mut state, "https://api.acme.example/v1");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "m");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        // Wire id with whitespace is rejected in the wizard, like shell-side.
        type_text(&mut state, "has space");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomWireModel));
        assert!(state.error.is_some());
    }

    #[test]
    fn custom_backend_selection_sticks() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_openai(&mut state);
        type_text(&mut state, "acme");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "https://api.acme.example/v1");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "m");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomDisplayName));
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        assert_eq!(state.custom_backend, 1);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        type_text(&mut state, "K");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        let outcome = state.handle_key(&key(KeyCode::Enter), &no_detect);
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
                api_backend,
                ..
            }) => assert_eq!(api_backend, "responses"),
            other => panic!("expected custom confirm, got {other:?}"),
        }
    }

    #[test]
    fn custom_messages_gateway_keeps_bearer() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_messages(&mut state, &no_detect);
        // Default row is the gateway: straight to the credential step.
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        type_text(&mut state, "GW_KEY");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        let outcome = state.handle_key(&key(KeyCode::Enter), &no_detect);
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
                api_backend,
                auth_scheme,
                anthropic_version,
                ..
            }) => {
                assert_eq!(api_backend, "messages");
                assert!(auth_scheme.is_none());
                assert_eq!(anthropic_version, "");
            }
            other => panic!("expected custom confirm, got {other:?}"),
        }
    }

    #[test]
    fn custom_messages_direct_emits_x_api_key_and_version() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_messages(&mut state, &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomAnthropicVersion));
        // Default version is prefilled; accept it.
        assert_eq!(
            state.input.text(),
            xai_grok_shell::util::config::DEFAULT_ANTHROPIC_VERSION
        );
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        type_text(&mut state, "ANTHROPIC_API_KEY");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomConfirm));
        let outcome = state.handle_key(&key(KeyCode::Enter), &no_detect);
        match outcome {
            SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
                api_backend,
                auth_scheme,
                anthropic_version,
                env_key,
                ..
            }) => {
                assert_eq!(api_backend, "messages");
                assert_eq!(auth_scheme.as_deref(), Some("x_api_key"));
                assert_eq!(
                    anthropic_version,
                    xai_grok_shell::util::config::DEFAULT_ANTHROPIC_VERSION
                );
                assert_eq!(env_key.as_deref(), Some("ANTHROPIC_API_KEY"));
            }
            other => panic!("expected custom confirm, got {other:?}"),
        }
    }

    #[test]
    fn custom_direct_back_nav_preserves_credential_draft() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_messages(&mut state, &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "PARTIAL_KEY");
        // Esc stashes the draft and returns to the version step.
        state.handle_key(&key(KeyCode::Esc), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomAnthropicVersion));
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        assert_eq!(state.input.text(), "PARTIAL_KEY");
    }

    #[test]
    fn quoted_env_name_blocked_like_shell() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_openai(&mut state);
        type_text(&mut state, "acme");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "https://api.acme.example/v1");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        type_text(&mut state, "m");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomKey));
        type_text(&mut state, "BAD\"NAME");
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomConfirm));
        let outcome = state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(state.error.is_some());
    }

    #[test]
    fn empty_env_name_blocks_confirm() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        state.set_input("   ");
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(state.error.is_some());
    }

    fn render_mouse_layout(state: &mut SetupWizardState) -> SetupWizardHitRects {
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render_setup_wizard(area, &mut buf, state);
        state.hit_areas.clone().expect("dialog should render")
    }

    fn no_detect(_: &str) -> SetupEnvPresence {
        SetupEnvPresence::default()
    }

    #[test]
    fn mouse_hover_moves_selection() {
        let mut state = SetupWizardState::new(options());
        let hits = render_mouse_layout(&mut state);
        assert_eq!(hits.rows.len(), 3);
        // Hover row 1 moves selection from 0 -> 1.
        let row = hits.rows[1];
        let outcome = state.handle_mouse(MouseEventKind::Moved, row.x + 1, row.y, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 1 }
        ));
        // Hover same row again: no change.
        let outcome = state.handle_mouse(MouseEventKind::Moved, row.x + 1, row.y, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        // Hover outside rows: no change, no panic.
        let outcome = state.handle_mouse(MouseEventKind::Moved, 0, 0, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 1 }
        ));
    }

    #[test]
    fn mouse_click_selects_then_confirms() {
        let mut state = SetupWizardState::new(options());
        let hits = render_mouse_layout(&mut state);
        // Click unselected row 1: selects.
        let row = hits.rows[1];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 1 }
        ));
        // Click selected row 1 again: confirms into EnterKey.
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[1];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::EnterKey { vendor_index: 1 }
        ));
    }

    #[test]
    fn mouse_click_outside_dialog_is_swallowed() {
        let mut state = SetupWizardState::new(options());
        let hits = render_mouse_layout(&mut state);
        // Outside the dialog (top-left corner).
        let outcome = state.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 0 }
        ));
        // Inside the dialog but off the rows (title row): also swallowed.
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.dialog.x + 1,
            hits.dialog.y + 1,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 0 }
        ));
    }

    #[test]
    fn mouse_without_render_is_swallowed() {
        let mut state = SetupWizardState::new(options());
        assert!(state.hit_areas.is_none());
        let outcome = state.handle_mouse(MouseEventKind::Moved, 10, 10, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        let outcome =
            state.handle_mouse(MouseEventKind::Down(MouseButton::Left), 10, 10, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected: 0 }
        ));
    }

    #[test]
    fn mouse_custom_row_click_goes_to_provider_id() {
        let mut state = SetupWizardState::new(options());
        let hits = render_mouse_layout(&mut state);
        let custom = hits.rows.len() - 1;
        // Hover custom row moves selection.
        let row = hits.rows[custom];
        let outcome = state.handle_mouse(MouseEventKind::Moved, row.x + 1, row.y, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected } if selected == custom
        ));
        // Click the selected custom row jumps straight to CustomProviderId.
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[custom];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomProviderId));
        // Esc returns to the custom row.
        state.handle_key(&key(KeyCode::Esc), &no_detect);
        assert!(matches!(
            state.step,
            SetupStep::ChooseVendor { selected } if selected == custom
        ));
    }

    #[test]
    fn mouse_custom_backend_hover_and_click() {
        let mut state = SetupWizardState::new(options());
        state.step = SetupStep::CustomBackend { selected: 0 };
        state.custom_backend = 0;
        let hits = render_mouse_layout(&mut state);
        assert!(!hits.rows.is_empty());
        let last = hits.rows.len() - 1;
        let row = hits.rows[last];
        let outcome = state.handle_mouse(MouseEventKind::Moved, row.x + 1, row.y, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::CustomBackend { selected } if selected == last
        ));
        assert_eq!(state.custom_backend, last);
        // Click selected backend row -> CustomKey for chat, branch for messages.
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[last];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomMessagesAuth { .. }));
        // First row (chat_completions) still goes straight to the key step.
        let mut state = SetupWizardState::new(options());
        state.step = SetupStep::CustomBackend { selected: 1 };
        state.custom_backend = 1;
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[0];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::CustomBackend { selected: 0 }
        ));
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[0];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomKey));
    }

    #[test]
    fn mouse_messages_auth_hover_and_click() {
        let mut state = SetupWizardState::new(options());
        state.step = SetupStep::CustomMessagesAuth { selected: 0 };
        let hits = render_mouse_layout(&mut state);
        assert_eq!(hits.rows.len(), 2);
        let row = hits.rows[1];
        let outcome = state.handle_mouse(MouseEventKind::Moved, row.x + 1, row.y, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(
            state.step,
            SetupStep::CustomMessagesAuth { selected: 1 }
        ));
        // Click the selected direct row -> version step.
        let hits = render_mouse_layout(&mut state);
        let row = hits.rows[1];
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.x + 1,
            row.y,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomAnthropicVersion));
        assert!(state.custom_messages_direct);
    }

    #[test]
    fn mouse_non_list_steps_swallow() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        assert!(matches!(state.step, SetupStep::EnterKey { .. }));
        let hits = render_mouse_layout(&mut state);
        assert!(hits.rows.is_empty());
        // Hover and click inside the dialog change nothing but are consumed.
        let outcome = state.handle_mouse(
            MouseEventKind::Moved,
            hits.dialog.x + 5,
            hits.dialog.y + 5,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        let outcome = state.handle_mouse(
            MouseEventKind::Down(MouseButton::Left),
            hits.dialog.x + 5,
            hits.dialog.y + 5,
            &no_detect,
        );
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        assert!(matches!(state.step, SetupStep::EnterKey { .. }));
        // Scroll is also swallowed, never panics.
        let outcome = state.handle_mouse(MouseEventKind::ScrollUp, 0, 0, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        let outcome = state.handle_mouse(MouseEventKind::ScrollDown, 0, 0, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
    }

    #[test]
    fn small_terminal_clears_hits_without_panic() {
        let mut state = SetupWizardState::new(options());
        let _ = render_mouse_layout(&mut state);
        assert!(state.hit_areas.is_some());
        let small = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(small);
        render_setup_wizard(small, &mut buf, &mut state);
        assert!(state.hit_areas.is_none());
        // Mouse after the fallback render must not panic and stays swallowed.
        let outcome = state.handle_mouse(MouseEventKind::Moved, 5, 5, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
        let outcome = state.handle_mouse(MouseEventKind::Down(MouseButton::Left), 5, 5, &no_detect);
        assert!(matches!(outcome, SetupWizardOutcome::Unchanged));
    }
}
