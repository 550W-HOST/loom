//! Server-local settings and UI preferences.
//!
//! Settings are deliberately separate from the domain registry and from
//! provider sessions. They describe the client/server installation, not a
//! thread or a run, so changing one never creates a relay event or interrupts
//! execution. The snapshot form is versioned independently from the domain
//! snapshot so older snapshots can acquire new defaults without losing the
//! entity view.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The current settings payload version.
pub const SETTINGS_VERSION: u32 = 1;

const UI_PREFERENCE_KEYS: [&str; 16] = [
    "sidebar.organizationMode",
    "sidebar.chronologicalSort",
    "sidebar.sortDirection",
    "sidebar.sectionOrder",
    "sidebar.manualSectionOrder",
    "sidebar.machineSectionOrder",
    "sidebar.collapsedSections",
    "sidebar.collapsedProjects",
    "sidebar.collapsedThreads",
    "sidebar.collapsedEnvironments",
    "sidebar.collapsedThreadSections",
    "sidebar.collapsedMachines",
    "sidebar.pluginPanelOrder",
    "sidebar.visiblePluginPanels",
    "sidebar.navigationProvider",
    "sidebar.threadListProvider",
];

/// The code-theme projection required by bb's appearance response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCodeTheme {
    pub dark: String,
    pub light: String,
    #[serde(default)]
    pub files: BTreeMap<String, Value>,
}

/// The active appearance selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppearanceSettings {
    pub theme_id: String,
    pub custom_css: Option<String>,
    pub favicon_color: String,
    pub resolved_code_theme: ResolvedCodeTheme,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme_id: "default".into(),
            custom_css: None,
            favicon_color: "default".into(),
            resolved_code_theme: ResolvedCodeTheme::default(),
        }
    }
}

/// Feature switches persisted by `system.experiments`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExperimentSettings {
    pub changelog_preview: bool,
    pub mobile_app: bool,
    pub sidebar_progressive_disclosure: bool,
    pub timeline_windowing: bool,
}

impl Default for ExperimentSettings {
    fn default() -> Self {
        Self {
            changelog_preview: false,
            mobile_app: false,
            sidebar_progressive_disclosure: false,
            timeline_windowing: true,
        }
    }
}

/// General client settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralSettings {
    pub show_keyboard_hints: bool,
    pub steer_active_thread_on_enter: bool,
    pub show_diagnostic_events: bool,
    pub provider_order: Vec<String>,
    pub default_provider_id: Option<String>,
    pub streamer_mode: bool,
    pub managed_branch_prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_unhandled_provider_events: Option<bool>,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            show_keyboard_hints: true,
            steer_active_thread_on_enter: true,
            show_diagnostic_events: false,
            provider_order: Vec::new(),
            default_provider_id: None,
            streamer_mode: false,
            managed_branch_prefix: String::new(),
            show_unhandled_provider_events: None,
        }
    }
}

/// One optimistic-concurrency controlled UI preference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPreference {
    pub revision: u64,
    pub value: Value,
}

/// Versioned settings data carried inside the durable domain snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    #[serde(default = "current_settings_version")]
    pub version: u32,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    #[serde(default)]
    pub experiments: ExperimentSettings,
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub keyboard: Vec<Value>,
    #[serde(default)]
    pub ui_preferences: BTreeMap<String, UiPreference>,
}

fn current_settings_version() -> u32 {
    SETTINGS_VERSION
}

impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings::default(),
            experiments: ExperimentSettings::default(),
            general: GeneralSettings::default(),
            keyboard: Vec::new(),
            ui_preferences: default_ui_preferences(),
        }
    }
}

/// The error returned by an atomic UI preference mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreferenceUpdateError {
    UnknownKey,
    InvalidValue,
    RevisionConflict { expected: u64, actual: u64 },
    RevisionExhausted,
}

/// In-memory settings with a single lock covering each update and its
/// revision check. The durable snapshot is written by the HTTP layer after a
/// successful mutation.
#[derive(Debug)]
pub struct SettingsRegistry {
    inner: Mutex<SettingsSnapshot>,
}

impl SettingsRegistry {
    /// Creates defaults for the provider configured on this server.
    pub fn new(provider_id: &str) -> Self {
        let mut snapshot = SettingsSnapshot::default();
        snapshot.general.provider_order = vec![provider_id.to_owned()];
        snapshot.general.default_provider_id = Some(provider_id.to_owned());
        Self {
            inner: Mutex::new(snapshot),
        }
    }

    /// Returns a stable copy for a response or a snapshot.
    pub fn export(&self) -> SettingsSnapshot {
        self.lock().clone()
    }

    /// Restores settings and applies additive migrations from older snapshots.
    pub fn restore(&self, mut snapshot: SettingsSnapshot, provider_id: &str) {
        // Version 0 was never written by loom, but treating it as the first
        // schema makes hand-authored/early snapshots forward compatible.
        let migrating = snapshot.version == 0;
        if snapshot.version == 0 {
            snapshot.version = SETTINGS_VERSION;
        }
        if snapshot.version > SETTINGS_VERSION {
            // A newer writer may have fields this binary cannot understand.
            // Keep the server usable with known defaults rather than silently
            // interpreting an incompatible representation.
            snapshot = SettingsSnapshot::default();
        }
        normalize_snapshot(&mut snapshot, provider_id, migrating);
        *self.lock() = snapshot;
    }

    pub fn appearance(&self) -> AppearanceSettings {
        self.lock().appearance.clone()
    }

    pub fn set_appearance(&self, theme_id: String, favicon_color: String) -> AppearanceSettings {
        let mut state = self.lock();
        state.appearance = AppearanceSettings {
            theme_id,
            custom_css: None,
            favicon_color,
            resolved_code_theme: ResolvedCodeTheme::default(),
        };
        state.appearance.clone()
    }

    pub fn experiments(&self) -> ExperimentSettings {
        self.lock().experiments.clone()
    }

    pub fn set_experiments(&self, experiments: ExperimentSettings) -> ExperimentSettings {
        let mut state = self.lock();
        state.experiments = experiments;
        state.experiments.clone()
    }

    pub fn general(&self) -> GeneralSettings {
        self.lock().general.clone()
    }

    pub fn set_general(&self, general: GeneralSettings) -> GeneralSettings {
        let mut state = self.lock();
        state.general = general;
        state.general.clone()
    }

    pub fn keyboard(&self) -> Vec<Value> {
        self.lock().keyboard.clone()
    }

    pub fn set_keyboard(&self, keyboard: Vec<Value>) -> Vec<Value> {
        let mut state = self.lock();
        state.keyboard = keyboard;
        state.keyboard.clone()
    }

    pub fn ui_preferences(&self) -> BTreeMap<String, UiPreference> {
        self.lock().ui_preferences.clone()
    }

    pub fn update_ui_preference(
        &self,
        key: &str,
        expected_revision: u64,
        value: Value,
    ) -> Result<UiPreference, PreferenceUpdateError> {
        if !is_known_ui_preference(key) {
            return Err(PreferenceUpdateError::UnknownKey);
        }
        if !valid_ui_preference_value(key, &value) {
            return Err(PreferenceUpdateError::InvalidValue);
        }
        let mut state = self.lock();
        let current = state
            .ui_preferences
            .get(key)
            .cloned()
            .unwrap_or_else(|| UiPreference {
                revision: 0,
                value: default_ui_preference(key).expect("known preference"),
            });
        if current.revision != expected_revision {
            return Err(PreferenceUpdateError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(PreferenceUpdateError::RevisionExhausted)?;
        let updated = UiPreference { revision, value };
        state.ui_preferences.insert(key.to_owned(), updated.clone());
        Ok(updated)
    }

    pub fn reset_ui_preference(&self, key: &str) -> Result<UiPreference, PreferenceUpdateError> {
        if !is_known_ui_preference(key) {
            return Err(PreferenceUpdateError::UnknownKey);
        }
        let mut state = self.lock();
        let current = state
            .ui_preferences
            .get(key)
            .cloned()
            .unwrap_or_else(|| UiPreference {
                revision: 0,
                value: default_ui_preference(key).expect("known preference"),
            });
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(PreferenceUpdateError::RevisionExhausted)?;
        let updated = UiPreference {
            revision,
            value: default_ui_preference(key).expect("known preference"),
        };
        state.ui_preferences.insert(key.to_owned(), updated.clone());
        Ok(updated)
    }

    fn lock(&self) -> MutexGuard<'_, SettingsSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Returns a value suitable for `system.config` and the appearance routes.
pub fn appearance_value(settings: &AppearanceSettings) -> Value {
    serde_json::to_value(settings).expect("appearance settings are serializable")
}

/// Returns the response projection for general settings.
pub fn general_value(settings: &GeneralSettings) -> Value {
    serde_json::to_value(settings).expect("general settings are serializable")
}

/// Returns the response projection for experiments.
pub fn experiments_value(settings: &ExperimentSettings) -> Value {
    serde_json::to_value(settings).expect("experiment settings are serializable")
}

/// Returns the built-in default theme. Custom/plugin themes are intentionally
/// not claimed until a real theme source is configured.
pub fn default_theme_value() -> Value {
    appearance_value(&AppearanceSettings::default())
}

/// Whether a theme id can be resolved by this server.
pub fn is_known_theme(theme_id: &str) -> bool {
    theme_id == "default"
}

/// Whether a favicon color is part of the bb appearance vocabulary.
pub fn is_valid_favicon_color(color: &str) -> bool {
    matches!(
        color,
        "default" | "red" | "orange" | "yellow" | "green" | "teal" | "blue" | "purple" | "pink"
    )
}

/// Whether a preference key is part of the bb contract.
pub fn is_known_ui_preference(key: &str) -> bool {
    UI_PREFERENCE_KEYS.contains(&key)
}

/// The initial value used when a preference is absent or reset.
pub fn default_ui_preference(key: &str) -> Option<Value> {
    let value = match key {
        "sidebar.organizationMode" => json!("project"),
        "sidebar.chronologicalSort" => json!("none"),
        "sidebar.sortDirection" => json!("default"),
        "sidebar.sectionOrder"
        | "sidebar.manualSectionOrder"
        | "sidebar.machineSectionOrder"
        | "sidebar.collapsedProjects"
        | "sidebar.collapsedThreads"
        | "sidebar.collapsedEnvironments"
        | "sidebar.collapsedThreadSections"
        | "sidebar.collapsedMachines"
        | "sidebar.pluginPanelOrder" => json!([]),
        "sidebar.collapsedSections" => json!([]),
        "sidebar.visiblePluginPanels" => Value::Null,
        "sidebar.navigationProvider" | "sidebar.threadListProvider" => json!("default"),
        _ => return None,
    };
    Some(value)
}

/// The preference value validator is intentionally stricter than the route's
/// generic `value` schema. It prevents a valid JSON value of the wrong type
/// from becoming a persistent value the UI cannot interpret.
pub fn valid_ui_preference_value(key: &str, value: &Value) -> bool {
    match key {
        "sidebar.organizationMode" => {
            matches!(
                value.as_str(),
                Some("project" | "chronological" | "machine")
            )
        }
        "sidebar.chronologicalSort" => {
            matches!(
                value.as_str(),
                Some("none" | "updated" | "created" | "alpha")
            )
        }
        "sidebar.sortDirection" => {
            matches!(value.as_str(), Some("default" | "ascending" | "descending"))
        }
        "sidebar.collapsedSections" => value.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| matches!(item.as_str(), Some("pinned" | "threads")))
        }),
        "sidebar.visiblePluginPanels" => value.is_null() || string_array(value),
        "sidebar.navigationProvider" | "sidebar.threadListProvider" => value.is_string(),
        _ if is_known_ui_preference(key) => string_array(value),
        _ => false,
    }
}

fn string_array(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(Value::is_string))
}

fn default_ui_preferences() -> BTreeMap<String, UiPreference> {
    UI_PREFERENCE_KEYS
        .into_iter()
        .map(|key| {
            (
                key.to_owned(),
                UiPreference {
                    revision: 0,
                    value: default_ui_preference(key).expect("every key has a default"),
                },
            )
        })
        .collect()
}

fn normalize_snapshot(snapshot: &mut SettingsSnapshot, provider_id: &str, migrating: bool) {
    snapshot.version = SETTINGS_VERSION;
    if snapshot.appearance.theme_id.is_empty() || !is_known_theme(&snapshot.appearance.theme_id) {
        snapshot.appearance = AppearanceSettings::default();
    }
    if !is_valid_favicon_color(&snapshot.appearance.favicon_color) {
        snapshot.appearance.favicon_color = "default".into();
    }
    if migrating && snapshot.general.provider_order.is_empty() {
        snapshot.general.provider_order.push(provider_id.to_owned());
        if snapshot.general.default_provider_id.is_none() {
            snapshot.general.default_provider_id = Some(provider_id.to_owned());
        }
    }
    for key in UI_PREFERENCE_KEYS {
        let default = default_ui_preference(key).expect("every key has a default");
        let entry = snapshot
            .ui_preferences
            .entry(key.to_owned())
            .or_insert_with(|| UiPreference {
                revision: 0,
                value: default.clone(),
            });
        if !valid_ui_preference_value(key, &entry.value) {
            entry.value = default;
        }
    }
    snapshot
        .ui_preferences
        .retain(|key, _| is_known_ui_preference(key));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_contract_preference() {
        let state = SettingsRegistry::new("pi");
        let preferences = state.ui_preferences();
        assert_eq!(preferences.len(), UI_PREFERENCE_KEYS.len());
        assert!(preferences
            .values()
            .all(|preference| valid_ui_preference_value(
                "sidebar.sectionOrder",
                &preference.value
            ) || preference.value.is_string()
                || preference.value.is_null()));
    }

    #[test]
    fn preference_revision_check_is_atomic() {
        let state = SettingsRegistry::new("pi");
        let updated = state
            .update_ui_preference("sidebar.sortDirection", 0, json!("ascending"))
            .unwrap();
        assert_eq!(updated.revision, 1);
        assert_eq!(updated.value, json!("ascending"));
        assert_eq!(
            state.update_ui_preference("sidebar.sortDirection", 0, json!("descending")),
            Err(PreferenceUpdateError::RevisionConflict {
                expected: 0,
                actual: 1
            })
        );
    }

    #[test]
    fn a_snapshot_without_settings_migrates_to_defaults() {
        let state = SettingsRegistry::new("acp");
        let snapshot = SettingsSnapshot {
            version: 0,
            ui_preferences: BTreeMap::new(),
            ..SettingsSnapshot::default()
        };
        state.restore(snapshot, "acp");
        let restored = state.export();
        assert_eq!(restored.version, SETTINGS_VERSION);
        assert_eq!(restored.general.provider_order, vec!["acp"]);
        assert_eq!(restored.ui_preferences.len(), UI_PREFERENCE_KEYS.len());
    }
}
