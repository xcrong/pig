//! First-run provider setup wizard.
//!
//! Shown on the welcome screen when no credentials are configured
//! (`auth_methods` empty). Lists builtin vendors from the shell's `VENDORS`
//! table so new snapshots appear automatically, detects the suggested
//! `env_key` only after the user picks a vendor (presence + length, value
//! never logged or echoed), and persists via the trusted `config.toml`
//! rewrite path. Custom providers get a step-by-step form for an
//! OpenAI-compatible endpoint (provider id, base URL, model, backend,
//! credential) plus a hint for the advanced pi-snapshot mirror path.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    ChooseVendor { selected: usize },
    EnterKey { vendor_index: usize },
    CustomMenu { selected: usize },
    CustomProviderId,
    CustomBaseUrl,
    CustomModelKey,
    CustomWireModel,
    CustomBackend { selected: usize },
    CustomKey,
    CustomConfirm,
    SnapshotHint,
    Saving,
    Done {
        config_path: String,
        follow_up: Option<String>,
    },
    Failed { error: String },
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
    pub custom_backend: usize,
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
        api_backend: String,
        env_key: Option<String>,
        api_key: Option<String>,
    },
}

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
            custom_backend: 0,
        }
    }

    pub fn custom_menu_len() -> usize {
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
                        self.step = SetupStep::CustomMenu { selected: 0 };
                        self.error = None;
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
            SetupStep::CustomMenu { mut selected } => match key.code {
                KeyCode::Esc => {
                    self.step = SetupStep::ChooseVendor {
                        selected: self.vendors.len(),
                    };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.step = SetupStep::CustomMenu { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Down => {
                    selected = (selected + 1).min(Self::custom_menu_len().saturating_sub(1));
                    self.step = SetupStep::CustomMenu { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    if selected == 0 {
                        self.input.set_text(&self.custom_provider_id);
                        self.error = None;
                        self.step = SetupStep::CustomProviderId;
                    } else {
                        self.error = None;
                        self.step = SetupStep::SnapshotHint;
                    }
                    SetupWizardOutcome::Changed
                }
                _ => {
                    if let Some(outcome) =
                        handle_list_nav(key, &mut selected, Self::custom_menu_len())
                    {
                        self.step = SetupStep::CustomMenu { selected };
                        return outcome;
                    }
                    SetupWizardOutcome::Unchanged
                }
            },
            SetupStep::CustomProviderId => match key.code {
                KeyCode::Esc => {
                    self.step = SetupStep::CustomMenu { selected: 0 };
                    self.error = None;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let value = self.input.text().trim().to_string();
                    if !valid_table_key(&value) {
                        self.error = Some("Use [A-Za-z0-9_-], max 64.".to_string());
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
                        self.error = Some("Must start with http(s)://.".to_string());
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
                        self.error = Some("Use [A-Za-z0-9_-], max 64.".to_string());
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
                    self.custom_wire_model = self.input.text().trim().to_string();
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
                    self.input.set_text(&self.custom_wire_model);
                    self.error = None;
                    self.step = SetupStep::CustomWireModel;
                    SetupWizardOutcome::Changed
                }
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    self.custom_backend = selected;
                    self.step = SetupStep::CustomBackend { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Down => {
                    selected =
                        (selected + 1).min(custom_backend_ids().len().saturating_sub(1));
                    self.custom_backend = selected;
                    self.step = SetupStep::CustomBackend { selected };
                    SetupWizardOutcome::Changed
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    self.custom_backend = selected;
                    self.start_custom_key(detect);
                    SetupWizardOutcome::Changed
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
            SetupStep::CustomKey => match key.code {
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
            SetupStep::SnapshotHint => match key.code {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.step = SetupStep::CustomMenu { selected: 1 };
                    SetupWizardOutcome::Changed
                }
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
        self.refresh_detection(detect);
        self.error = None;
        self.step = SetupStep::CustomKey;
    }

    pub fn insert_paste(
        &mut self,
        text: &str,
        detect: &impl Fn(&str) -> SetupEnvPresence,
    ) -> SetupWizardOutcome {
        if !matches!(
            self.step,
            SetupStep::EnterKey { .. } | SetupStep::CustomKey
        ) {
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
                    self.error = Some("Enter an env var name.".to_string());
                    return None;
                }
                if name.contains(char::is_whitespace) || name.contains('=') {
                    self.error = Some("Env var name looks invalid.".to_string());
                    return None;
                }
                Some((Some(name), None))
            }
            SetupKeyMode::PasteKey => {
                let secret = self.input.text().trim().to_string();
                if secret.is_empty() {
                    self.error = Some("Paste an API key.".to_string());
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
            self.error = Some("Missing provider details; go back.".to_string());
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
        self.error = None;
        SetupWizardOutcome::Confirm(SetupConfirmRequest::CustomProvider {
            provider_id: self.custom_provider_id.trim().to_string(),
            base_url: self.custom_base_url.trim().to_string(),
            model_key: self.custom_model_key.trim().to_string(),
            wire_model: self.custom_wire_model.trim().to_string(),
            api_backend: backend,
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

fn dialog_width_for(area_width: u16) -> u16 {
    DIALOG_MIN_WIDTH.min(area_width.saturating_sub(4)).max(20)
}

/// Render the wizard centered over the welcome screen.
pub fn render_setup_wizard(area: Rect, buf: &mut Buffer, state: &SetupWizardState) {
    let theme = Theme::current();
    if area.height < DIALOG_HEIGHT || area.width < 30 {
        if area.height >= 1 && area.width >= 16 {
            let hint = Line::from(Span::styled(
                "[Esc] to close",
                Style::default().fg(theme.gray_dim),
            ));
            hint.render(Rect::new(area.x, area.y, area.width.min(16), 1), buf);
        }
        return;
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
        SetupStep::CustomMenu { selected } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Custom provider:",
                    Style::default().fg(theme.gray_bright),
                )),
            );
            for (i, label) in [
                "OpenAI-compatible endpoint (guided)",
                "pi snapshot mirror (advanced)",
            ]
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
        SetupStep::CustomProviderId => {
            render_custom_field(
                buf, inner_x, &mut row, inner_w, &theme, state,
                "Step 1/6 — Provider id",
                "Short name for [model_providers.<id>]:",
                "Provider id: ",
            );
        }
        SetupStep::CustomBaseUrl => {
            render_custom_field(
                buf, inner_x, &mut row, inner_w, &theme, state,
                "Step 2/6 — Base URL",
                "OpenAI-compatible endpoint (https://…/v1):",
                "Base URL: ",
            );
        }
        SetupStep::CustomModelKey => {
            render_custom_field(
                buf, inner_x, &mut row, inner_w, &theme, state,
                "Step 3/6 — Model",
                "Catalog key for [model.<key>] and /model:",
                "Model: ",
            );
        }
        SetupStep::CustomWireModel => {
            render_custom_field(
                buf, inner_x, &mut row, inner_w, &theme, state,
                "Step 4/6 — Wire model id (optional)",
                "Id sent to the API. Empty = same as key:",
                "Wire id: ",
            );
        }
        SetupStep::CustomBackend { selected } => {
            paint_line(
                buf,
                inner_x,
                &mut row,
                inner_w,
                Line::from(Span::styled(
                    "Step 5/6 — API backend:",
                    Style::default().fg(theme.gray_bright),
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
            let cred = match state.key_mode {
                SetupKeyMode::UseEnv => format!("env_key = \"{}\"", state.input.text().trim()),
                SetupKeyMode::PasteKey => "api_key = ••• (hidden)".to_string(),
            };
            for line in [
                Line::from(Span::styled(
                    "Step 6/6 — Confirm:",
                    Style::default().fg(theme.gray_bright),
                )),
                Line::from(Span::styled(
                    format!(
                        "[model_providers.{}] {}",
                        state.custom_provider_id.trim(),
                        state.custom_base_url.trim()
                    ),
                    Style::default().fg(theme.text_primary),
                )),
                Line::from(Span::styled(
                    format!("[model.{}] {} via {}", state.custom_model_key.trim(), wire, backend),
                    Style::default().fg(theme.text_primary),
                )),
                Line::from(Span::styled(cred, Style::default().fg(theme.gray))),
                Line::from(vec![
                    Span::styled("enter", bold_accent(&theme)),
                    Span::styled(" = save   ", Style::default().fg(theme.gray)),
                    Span::styled("esc", bold_accent(&theme)),
                    Span::styled(" = back", Style::default().fg(theme.gray)),
                ]),
            ] {
                paint_line(buf, inner_x, &mut row, inner_w, line);
            }
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
        SetupStep::SnapshotHint => {
            for line in [
                Line::from(Span::styled(
                    "pi snapshot mirror (advanced)",
                    Style::default().fg(theme.text_primary),
                )),
                Line::from(Span::styled(
                    "Save pi.dev /api/models/providers/<id>",
                    Style::default().fg(theme.gray),
                )),
                Line::from(Span::styled(
                    "to ~/.config/pig/vendors/<id>.json, then:",
                    Style::default().fg(theme.gray),
                )),
                Line::from(Span::styled(
                    "[vendors.acme] base_url + snapshot_file",
                    Style::default().fg(theme.gray_bright),
                )),
                Line::from(Span::styled(
                    "+ env_key. See 11-custom-models.md.",
                    Style::default().fg(theme.gray_dim),
                )),
                Line::from(vec![
                    Span::styled("esc", bold_accent(&theme)),
                    Span::styled(" = back", Style::default().fg(theme.gray)),
                ]),
            ] {
                paint_line(buf, inner_x, &mut row, inner_w, line);
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
                    Line::from(Span::styled(short_hint, Style::default().fg(theme.gray_bright))),
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
        SetupKeyMode::UseEnv => "Step 6/6 — Credential: env var (recommended)  [Tab] paste instead",
        SetupKeyMode::PasteKey => "Step 6/6 — Credential: paste key  [Tab] use env var instead",
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
    fn custom_row_opens_menu() {
        let mut state = SetupWizardState::new(options());
        state.handle_key(&key(KeyCode::Down), &|_| SetupEnvPresence::default());
        state.handle_key(&key(KeyCode::Down), &|_| SetupEnvPresence::default());
        let outcome = state.handle_key(&key(KeyCode::Enter), &|_| SetupEnvPresence::default());
        assert!(matches!(outcome, SetupWizardOutcome::Changed));
        assert!(matches!(state.step, SetupStep::CustomMenu { selected: 0 }));
    }

    fn enter_custom_openai(state: &mut SetupWizardState) {
        let no_detect = |_: &str| SetupEnvPresence::default();
        // ChooseVendor (custom row) -> CustomMenu -> OpenAI path.
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomMenu { .. }));
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::CustomProviderId));
    }

    fn type_text(state: &mut SetupWizardState, text: &str) {
        state.set_input(text);
    }

    #[test]
    fn custom_openai_flow_confirms_provider() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        enter_custom_openai(&mut state);
        // Provider id -> base URL -> model key -> wire (empty = key) -> backend.
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
                api_backend,
                env_key,
                api_key,
            }) => {
                assert_eq!(provider_id, "acme");
                assert_eq!(base_url, "https://api.acme.example/v1");
                assert_eq!(model_key, "acme-model");
                assert_eq!(wire_model, "", "empty wire defaults shell-side");
                assert_eq!(api_backend, "chat_completions");
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
    fn snapshot_row_shows_hint() {
        let mut state = SetupWizardState::new(options());
        let no_detect = |_: &str| SetupEnvPresence::default();
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        state.handle_key(&key(KeyCode::Down), &no_detect);
        state.handle_key(&key(KeyCode::Enter), &no_detect);
        assert!(matches!(state.step, SetupStep::SnapshotHint));
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
}
