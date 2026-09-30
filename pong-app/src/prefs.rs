//! The window's own settings, as opposed to the host's (which the host
//! keeps, and the window edits over its API): `window.toml` in this app's
//! folder. They are the signed-in user's, so they work where the host's
//! folder is closed to them (the service's, on Windows).

use std::path::{Path, PathBuf};

use pingpong_update::{Build, Channel};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// What the update check follows, once the user has chosen (until
    /// then, what suits the build that runs: see [`Prefs::update_channel`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updates: Option<Channel>,
}

fn path(dir: &Path) -> PathBuf {
    dir.join("window.toml")
}

impl Prefs {
    /// What the update check follows: the user's choice, else this build's
    /// default. The default is not saved, so a checkout's (main) does not
    /// follow the settings to a packaged release, whose default is releases.
    pub fn update_channel(&self) -> Channel {
        self.updates
            .unwrap_or_else(|| Channel::default_for(&Build::this()))
    }

    pub fn load(dir: &Path) -> Prefs {
        match std::fs::read_to_string(path(dir)) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "window.toml unreadable; using defaults");
                Prefs::default()
            }),
            Err(_) => Prefs::default(),
        }
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path(dir), text) {
                    tracing::warn!(error = %e, "the window's settings were not saved");
                }
            }
            Err(e) => tracing::warn!(error = %e, "the window's settings were not saved"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip_and_fill_in_missing_keys() {
        let prefs = Prefs {
            updates: Some(Channel::Off),
        };
        let text = toml::to_string_pretty(&prefs).unwrap();
        assert_eq!(text.trim(), "updates = \"off\"");
        assert_eq!(toml::from_str::<Prefs>(&text).unwrap(), prefs);
        assert_eq!(toml::from_str::<Prefs>("").unwrap(), Prefs::default());
        // Not chosen: nothing saved, and the build's own default applies.
        assert_eq!(
            toml::to_string_pretty(&Prefs::default()).unwrap().trim(),
            ""
        );
        assert_eq!(
            Prefs::default().update_channel(),
            Channel::default_for(&Build::this())
        );
    }
}
