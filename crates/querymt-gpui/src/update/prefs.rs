//! What the user chose about updates. These types are in every build (the
//! saved preferences have them on the web too); the update logic is native only.

use serde::{Deserialize, Serialize};

/// A release channel the user can follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Nightly,
}

impl Channel {
    // The preference types are in the web build, the update code that names a
    // channel is not.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
        }
    }
}

/// Saved with the other preferences. State files written before updates
/// existed have none of it and load with the defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePrefs {
    /// The channel picked in Settings; `None` follows the build's channel.
    #[serde(default)]
    pub channel: Option<Channel>,
    /// Check 30 seconds after start and every 6 hours.
    #[serde(default = "default_auto_check")]
    pub auto_check: bool,
    /// A version the user skipped: automatic checks do not announce it.
    #[serde(default)]
    pub skipped_version: Option<String>,
    /// When the last check finished, in seconds since the Unix epoch.
    #[serde(default)]
    pub last_check: Option<u64>,
}

fn default_auto_check() -> bool {
    true
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self {
            channel: None,
            auto_check: true,
            skipped_version: None,
            last_check: None,
        }
    }
}
