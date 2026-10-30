//! Settings persisted between runs (eframe stores them in the OS app-data dir:
//! `%APPDATA%\CC2 Monitor` on Windows, `~/.local/share/cc2monitor` on Linux).

use serde::{Deserialize, Serialize};

pub const STORAGE_KEY: &str = "settings";
pub const DEFAULT_ACCESS_CODE: &str = "123456";
fn default_access_code() -> String { DEFAULT_ACCESS_CODE.to_owned() }

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub ip: String,
    #[serde(skip, default = "default_access_code")]
    pub access_code: String,
    pub protected_access_code: String,
    pub serial: String,
    pub auto_connect: bool,
    pub camera_on: bool,
    /// Camera frame rate cap; 0 = as fast as the camera sends (~20 fps).
    pub camera_fps: u32,
    /// Camera resolution divisor: 1 full, 2 half, 4 quarter.
    pub camera_scale: u8,
    /// Last plate side used to start a print: false textured (A), true smooth (B).
    pub smooth_plate: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ip: String::new(),
            access_code: DEFAULT_ACCESS_CODE.into(),
            protected_access_code: String::new(),
            serial: String::new(),
            auto_connect: true,
            camera_on: true,
            camera_fps: 10,
            camera_scale: 1,
            smooth_plate: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_code_never_appears_in_saved_settings() {
        let mut settings = Settings::default();
        settings.access_code = "unique-test-code-secret".to_owned();
        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("unique-test-code-secret"));
        assert!(!serialized.contains(r#""access_code""#));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_for_current_user() {
        let protected = crate::secret::protect("test-token-123").expect("protect");
        assert_ne!(protected, "test-token-123");
        assert_eq!(crate::secret::unprotect(&protected).as_deref(), Some("test-token-123"));
    }
}
