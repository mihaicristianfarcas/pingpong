//! Host settings, and where the host keeps its state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The tunnel port. One UDP port carries every paired client.
pub const DEFAULT_PORT: u16 = 47800;
/// Pairing and host info (TCP).
pub const DEFAULT_PAIRING_PORT: u16 = 47801;
/// Web UI (HTTPS).
pub const DEFAULT_WEB_PORT: u16 = 47802;

/// `%ProgramData%\Pong` on Windows (shared by the service and the host it
/// launches), `~/Library/Application Support/Pong` on a Mac,
/// `$XDG_CONFIG_HOME/pong` (`~/.config/pong`) on Linux. `PONG_DATA_DIR`
/// overrides them.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PONG_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("Pong")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        home.join("Library/Application Support/Pong")
    }
    // Beside Ping's: $XDG_CONFIG_HOME/pong.
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = env("HOME").unwrap_or_else(std::env::temp_dir);
        env("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("pong")
    }
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        // Linux: not in a service's environment.
        .or_else(|_| {
            std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string())
        })
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Pong".into())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HostConfig {
    /// Shown to clients.
    pub name: String,
    pub port: u16,
    pub pairing_port: u16,
    pub web_port: u16,
    /// NVENC preset 1 (fastest) .. 7 (best quality).
    pub nvenc_preset: u8,
    /// Two-pass rate control at quarter resolution.
    pub nvenc_two_pass: bool,
    /// The NVIDIA driver runs the GPU at full power for Pong (a driver
    /// profile of Pong's own), as Sunshine's `nvenc_latency_over_power`.
    pub nvidia_max_power: bool,
    /// OpenGL and Vulkan games present through DXGI while Pong runs (a
    /// system-wide driver setting, put back when Pong stops), so they are
    /// captured at their full frame rate: Sunshine's
    /// `nvenc_opengl_vulkan_on_dxgi`.
    pub nvidia_dxgi_present: bool,
    /// Cap on what a client may request, kbit/s (0 = no cap).
    pub max_bitrate_kbps: u32,
    /// Cap on the stream frame rate (0 = the client decides).
    pub max_fps: u32,
    /// Allow HEVC / AV1 when the client supports them.
    pub allow_hevc: bool,
    pub allow_av1: bool,
    /// Ceiling on the video send rate, Mbit/s. Frames go out in 1 ms groups
    /// under this rate instead of as one burst (Apollo uses ~80% of 1 Gbit/s).
    pub pace_mbps: u32,
    /// Leave the host's own monitors on while streaming. Off by default, as
    /// in Apollo: the virtual display becomes the whole desktop, so windows
    /// and games open where the client can see them.
    pub keep_host_displays: bool,
    /// Let a client take over a session another client is running.
    pub allow_takeover: bool,
    /// Lower the bitrate when the network congests; climb back when it clears.
    pub adaptive_bitrate: bool,
    /// Publish this host's public address (sealed, for paired devices only)
    /// so they can connect from anywhere without port forwarding.
    pub internet_access: bool,
    /// Let paired AI agents start sessions (each agent's own access, set on
    /// the Devices page, applies too). Off: every agent is refused.
    pub agents: bool,
    /// Hold an agent's keyboard and mouse for this long after someone uses
    /// the host's own (seconds; 0: never hold).
    pub agent_local_input_hold_secs: u32,
    /// Ask the router to forward the tunnel's port (UPnP, NAT-PMP), as
    /// Apollo does: clients on the internet then reach the host at the
    /// router's public address. Only with `internet_access`.
    pub port_mapping: bool,
    /// Share the clipboard with a person's client that asks for it: text,
    /// images and files copied on one side can be pasted on the other.
    pub clipboard: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        HostConfig {
            name: default_name(),
            port: DEFAULT_PORT,
            pairing_port: DEFAULT_PAIRING_PORT,
            web_port: DEFAULT_WEB_PORT,
            nvenc_preset: 1,
            nvenc_two_pass: true,
            nvidia_max_power: true,
            nvidia_dxgi_present: true,
            max_bitrate_kbps: 0,
            max_fps: 0,
            allow_hevc: true,
            allow_av1: true,
            pace_mbps: 800,
            keep_host_displays: false,
            allow_takeover: true,
            adaptive_bitrate: true,
            internet_access: true,
            agents: true,
            agent_local_input_hold_secs: 10,
            port_mapping: true,
            clipboard: true,
        }
    }
}

impl HostConfig {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("config.toml")
    }

    /// Load `config.toml`, writing the defaults out if there is none so the
    /// user has a file to edit.
    pub fn load_or_default(dir: &Path) -> HostConfig {
        let path = Self::path(dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "config unreadable; using defaults");
                    HostConfig::default()
                }
            },
            Err(_) => {
                let c = HostConfig::default();
                let _ = c.save(dir);
                c
            }
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path(dir), text)?;
        // Pong's window reads the web port from it, and is not elevated.
        crate::private::make_public(&Self::path(dir));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_fills_in_defaults() {
        let c: HostConfig = toml::from_str("name = \"box\"\npace_mbps = 300\n").unwrap();
        assert_eq!(c.name, "box");
        assert_eq!(c.pace_mbps, 300);
        assert_eq!(c.port, DEFAULT_PORT);
    }

    #[test]
    fn defaults_are_written_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let a = HostConfig::load_or_default(dir.path());
        let b = HostConfig::load_or_default(dir.path());
        assert_eq!(a, b);
    }
}
