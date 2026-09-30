//! Starting a program when the user signs in: Pong's icon, which should be
//! in the tray without anyone opening it, and on a Mac the host itself.
//!
//! - macOS: a LaunchAgent in `~/Library/LaunchAgents`. Turning it on or off
//!   changes what the next sign-in does and nothing now (unloading would
//!   end the running app, were it launchd that started it);
//!   [`LoginItem::start_now`] is for starting it at once as well.
//! - Windows: a value under the user's own `Run` key.
//! - Linux: none. There is no tray there (see `tray`), so there is nothing
//!   to show at sign-in; the desktop's own "startup applications" can start
//!   the window if someone wants that.
//!
//! All of it is the signed-in user's own: nothing here needs an
//! administrator.

use std::path::Path;

/// What starts at login: `name` to the user ("Pong"), `id` to the system
/// (the LaunchAgent's label), and the arguments the program gets (so the
/// app starts without its window, the host as a host).
#[derive(Debug, Clone, Copy)]
pub struct LoginItem {
    pub id: &'static str,
    pub name: &'static str,
    pub args: &'static [&'static str],
    /// Started again whenever it exits (a host, not an app: an app the user
    /// quit stays quit). A Mac's LaunchAgent only; elsewhere services do
    /// this.
    pub keep_alive: bool,
}

/// Whether this system can start the app at login at all.
pub const fn supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

impl LoginItem {
    /// Whether `exe` starts at login as things stand.
    pub fn enabled(&self, exe: &Path) -> bool {
        platform::enabled(self, exe)
    }

    /// Make `exe` start at login, or stop it from doing so.
    pub fn set(&self, exe: &Path, on: bool) -> Result<(), String> {
        platform::set(self, exe, on)
    }

    /// Start it now, as the next sign-in would (after [`LoginItem::set`]):
    /// launchd loads the agent into this user's session. A Mac only: the
    /// other systems start their hosts as services.
    #[cfg(target_os = "macos")]
    pub fn start_now(&self) -> Result<(), String> {
        platform::start_now(self)
    }
}

/// `text` inside an XML element.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent that runs `exe` with the item's arguments at login.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launch_agent(item: &LoginItem, exe: &Path) -> String {
    let mut arguments = format!("\t\t<string>{}</string>\n", xml(&exe.to_string_lossy()));
    for arg in item.args {
        arguments.push_str(&format!("\t\t<string>{}</string>\n", xml(arg)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {arguments}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         {keep_alive}\
         \t<key>ProcessType</key>\n\
         \t<string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        label = xml(item.id),
        keep_alive = if item.keep_alive {
            "\t<key>KeepAlive</key>\n\t<true/>\n"
        } else {
            ""
        },
    )
}

/// The command line Windows runs at sign-in: the program quoted, then its
/// arguments.
#[cfg_attr(not(windows), allow(dead_code))]
fn run_command(item: &LoginItem, exe: &Path) -> String {
    let mut command = format!("\"{}\"", exe.display());
    for arg in item.args {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};

    use super::{launch_agent, xml, LoginItem};

    fn plist(item: &LoginItem) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(
            PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", item.id)),
        )
    }

    pub(super) fn enabled(item: &LoginItem, exe: &Path) -> bool {
        // The agent there must start this copy: one that starts another
        // copy of the app (an older install, elsewhere) is not this app
        // starting at login. Whoever wrote it (this app, or the install
        // script), it names the program the same way.
        let program = format!("<string>{}</string>", xml(&exe.to_string_lossy()));
        plist(item)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|text| text.contains(&program))
    }

    pub(super) fn set(item: &LoginItem, exe: &Path, on: bool) -> Result<(), String> {
        let path = plist(item).ok_or("There is no home folder to keep it in.")?;
        if on {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, launch_agent(item, exe)).map_err(|e| e.to_string())
        } else {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            }
        }
    }

    pub(super) fn start_now(item: &LoginItem) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let path = plist(item).ok_or("There is no home folder to keep it in.")?;
        // The user's own session is named by their number: the home
        // folder's owner.
        let home = std::env::var_os("HOME").ok_or("There is no home folder.")?;
        let uid = std::fs::metadata(home).map_err(|e| e.to_string())?.uid();
        let launchctl = |args: &[&str]| {
            std::process::Command::new("/bin/launchctl")
                .args(args)
                .output()
                .map_err(|e| format!("launchctl did not run: {e}"))
        };
        // An agent of this name already loaded (an older one) goes first:
        // launchd refuses to load a second. Not loaded is the usual case.
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{}", item.id)]);
        let out = launchctl(&["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "launchd did not start it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, REG_SZ, REG_VALUE_TYPE,
    };

    use super::{run_command, LoginItem};

    /// The signed-in user's programs to start at sign-in.
    fn run_key() -> Result<HKEY, String> {
        let mut key = HKEY::default();
        // SAFETY: `key` receives the opened key; the path is a constant.
        unsafe {
            RegCreateKeyW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
                &mut key,
            )
        }
        .ok()
        .map_err(|e| e.message())?;
        Ok(key)
    }

    /// What the `Run` key holds under the item's name.
    fn current(item: &LoginItem) -> Option<String> {
        let key = run_key().ok()?;
        let name = HSTRING::from(item.name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut buf = [0u16; 1024];
        let mut bytes = std::mem::size_of_val(&buf) as u32;
        // SAFETY: `buf` and `bytes` describe the same buffer; the key is
        // closed once.
        let status = unsafe {
            let status = RegQueryValueExW(
                key,
                &name,
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            );
            let _ = RegCloseKey(key);
            status
        };
        (status.is_ok() && kind == REG_SZ).then(|| {
            // The registry does not promise the terminating NUL.
            let units = &buf[..(bytes as usize / 2).min(buf.len())];
            let len = units.iter().position(|c| *c == 0).unwrap_or(units.len());
            String::from_utf16_lossy(&units[..len])
        })
    }

    pub(super) fn enabled(item: &LoginItem, exe: &Path) -> bool {
        current(item).is_some_and(|c| c.eq_ignore_ascii_case(&run_command(item, exe)))
    }

    pub(super) fn set(item: &LoginItem, exe: &Path, on: bool) -> Result<(), String> {
        let key = run_key()?;
        let name = HSTRING::from(item.name);
        // A string value is its UTF-16, little-endian, with the NUL.
        let command: Vec<u8> = run_command(item, exe)
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: the key is the one just opened, closed once; the name and
        // the value's bytes are alive for the calls.
        let status = unsafe {
            let status = if on {
                RegSetValueExW(key, &name, None, REG_SZ, Some(&command))
            } else {
                match RegDeleteValueW(key, &name) {
                    e if e == ERROR_FILE_NOT_FOUND => Default::default(),
                    other => other,
                }
            };
            let _ = RegCloseKey(key);
            status
        };
        status.ok().map_err(|e| e.message())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::path::Path;

    use super::LoginItem;

    pub(super) fn enabled(_: &LoginItem, _: &Path) -> bool {
        false
    }

    pub(super) fn set(_: &LoginItem, _: &Path, _: bool) -> Result<(), String> {
        Err("Starting at login is not set up from here on this system.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: LoginItem = LoginItem {
        id: "dev.example.App",
        name: "App",
        args: &["--background"],
        keep_alive: false,
    };

    #[test]
    fn the_launch_agent_runs_the_program_once_at_login_with_its_arguments() {
        let text = launch_agent(
            &ITEM,
            Path::new("/Applications/An App.app/Contents/MacOS/App"),
        );
        assert!(text.contains("<string>dev.example.App</string>"));
        assert!(text.contains("<string>/Applications/An App.app/Contents/MacOS/App</string>"));
        assert!(text.contains("<string>--background</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"));
        // An app the user quit stays quit.
        assert!(!text.contains("KeepAlive"));
        // A host is started again when it exits.
        let host = LoginItem {
            keep_alive: true,
            ..ITEM
        };
        let text = launch_agent(&host, Path::new("/Applications/App.app/Contents/MacOS/app"));
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
    }

    #[test]
    fn a_path_with_markup_in_it_stays_one_string() {
        let text = launch_agent(&ITEM, Path::new("/Users/a&b/<App>"));
        assert!(text.contains("<string>/Users/a&amp;b/&lt;App&gt;</string>"));
    }

    #[test]
    fn the_run_command_quotes_the_program_so_spaces_do_not_split_it() {
        assert_eq!(
            run_command(&ITEM, Path::new(r"C:\Program Files\App\The App.exe")),
            r#""C:\Program Files\App\The App.exe" --background"#
        );
    }
}
