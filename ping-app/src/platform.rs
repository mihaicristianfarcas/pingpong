//! The little the app needs from the operating system itself: reading
//! another app's preferences (Moonlight's; on a Mac also the SwiftUI Ping's).
//!
//! Only named keys are ever read: Moonlight keeps its client certificate and
//! key in the same place, and those are none of Ping's business.

/// Read-only access to one application's saved preferences.
pub struct Defaults {
    #[cfg(target_os = "macos")]
    domain: objc2_core_foundation::CFRetained<objc2_core_foundation::CFString>,
    #[cfg(windows)]
    key: windows::Win32::System::Registry::HKEY,
    #[cfg(not(any(target_os = "macos", windows)))]
    values: std::collections::HashMap<String, String>,
}

/// A preference's value, whatever form the platform kept it in.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Value {
    Text(String),
    Int(i64),
    Bool(bool),
}

impl Defaults {
    /// Moonlight's settings (Qt's QSettings for "Moonlight Game Streaming
    /// Project"/"Moonlight").
    pub fn moonlight() -> Option<Defaults> {
        #[cfg(target_os = "macos")]
        return Defaults::open("com.moonlight-stream.Moonlight");
        #[cfg(windows)]
        return Defaults::open(r"Software\Moonlight Game Streaming Project\Moonlight");
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or(home.join(".config"));
            Defaults::open(
                &config
                    .join("Moonlight Game Streaming Project/Moonlight.conf")
                    .to_string_lossy(),
            )
        }
    }

    /// A Mac application's domain ("com.example.App"); on Windows a key
    /// under HKEY_CURRENT_USER; elsewhere a QSettings .conf file.
    #[cfg(target_os = "macos")]
    pub fn open(domain: &str) -> Option<Defaults> {
        Some(Defaults {
            domain: objc2_core_foundation::CFString::from_str(domain),
        })
    }

    #[cfg(windows)]
    pub fn open(subkey: &str) -> Option<Defaults> {
        use windows::core::HSTRING;
        use windows::Win32::System::Registry::{RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ};
        let mut key = HKEY::default();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(subkey),
                None,
                KEY_READ,
                &mut key,
            )
        };
        status.is_ok().then_some(Defaults { key })
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn open(path: &str) -> Option<Defaults> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut values = std::collections::HashMap::new();
        let mut general = true;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                general = line == "[General]";
            } else if let (true, Some((k, v))) = (general, line.split_once('=')) {
                values.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
        }
        Some(Defaults { values })
    }

    #[cfg(target_os = "macos")]
    fn value(&self, key: &str) -> Option<Value> {
        use objc2_core_foundation::{CFBoolean, CFNumber, CFPreferencesCopyAppValue, CFString};
        let v = CFPreferencesCopyAppValue(&CFString::from_str(key), &self.domain)?;
        if let Some(b) = v.downcast_ref::<CFBoolean>() {
            return Some(Value::Bool(b.as_bool()));
        }
        if let Some(n) = v.downcast_ref::<CFNumber>() {
            return n.as_i64().map(Value::Int);
        }
        v.downcast_ref::<CFString>()
            .map(|s| Value::Text(s.to_string()))
    }

    #[cfg(windows)]
    fn value(&self, key: &str) -> Option<Value> {
        use windows::core::HSTRING;
        use windows::Win32::System::Registry::{
            RegQueryValueExW, REG_DWORD, REG_QWORD, REG_SZ, REG_VALUE_TYPE,
        };
        let name = HSTRING::from(key);
        let mut kind = REG_VALUE_TYPE::default();
        let mut buf = [0u8; 512];
        let mut len = buf.len() as u32;
        let status = unsafe {
            RegQueryValueExW(
                self.key,
                &name,
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr()),
                Some(&mut len),
            )
        };
        if status.is_err() {
            return None;
        }
        let bytes = &buf[..len as usize];
        match kind {
            REG_DWORD if bytes.len() >= 4 => Some(Value::Int(u32::from_le_bytes(
                bytes[..4].try_into().ok()?,
            ) as i32 as i64)),
            REG_QWORD if bytes.len() >= 8 => {
                Some(Value::Int(i64::from_le_bytes(bytes[..8].try_into().ok()?)))
            }
            REG_SZ => {
                let wide: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .take_while(|&c| c != 0)
                    .collect();
                Some(Value::Text(String::from_utf16_lossy(&wide)))
            }
            _ => None,
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn value(&self, key: &str) -> Option<Value> {
        self.values.get(key).map(|v| Value::Text(v.clone()))
    }

    pub fn has(&self, key: &str) -> bool {
        self.value(key).is_some()
    }

    pub fn string(&self, key: &str) -> Option<String> {
        match self.value(key)? {
            Value::Text(s) => Some(s),
            Value::Int(i) => Some(i.to_string()),
            Value::Bool(b) => Some(b.to_string()),
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.value(key)? {
            Value::Int(i) => Some(i),
            Value::Bool(b) => Some(b as i64),
            Value::Text(s) => s.trim().parse().ok(),
        }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.value(key)? {
            Value::Bool(b) => Some(b),
            Value::Int(i) => Some(i != 0),
            Value::Text(s) => match s.trim() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
        }
    }
}

#[cfg(windows)]
impl Drop for Defaults {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(self.key) };
    }
}
