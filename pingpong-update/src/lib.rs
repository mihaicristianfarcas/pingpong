//! Knowing when a newer Ping or Pong exists, so the apps can say so.
//!
//! Two things are followed, on GitHub, where the project lives:
//!
//! - **Releases**: the latest release's tag against this build's version.
//! - **The main branch**, for a build made from a checkout: whether `main`
//!   has commits this build's commit does not.
//!
//! GitHub's public API is asked without an account, on its own thread, a few
//! seconds after the app starts if the last answer is more than a day old,
//! and once a day after that. Nothing about this computer goes out beyond
//! what any HTTPS request shows (its address) and the program's version in
//! the `User-Agent`. The user can turn it off ([`Channel::Off`]); "Check for
//! Updates" then still asks once, because they asked.
//!
//! Nothing is downloaded or installed: the apps say what is there and how
//! to get it (the release's page, or the package manager's command when one
//! installed this copy).

mod github;
mod version;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub use version::Version;

/// Where the project lives (the workspace's `repository`).
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// GitHub is asked again when the last answer is this old. Releases are
/// days apart at the closest; an hourly check would only spend the 60
/// requests an hour GitHub allows an address.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The first check waits this long after the app starts, so it does not
/// compete with the launch (discovery, the window's first frames).
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(5);

/// After a failed check (no network, GitHub's allowance used up).
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// What this program was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// The program's version (`CARGO_PKG_VERSION`; every program in the
    /// workspace has the project's).
    pub version: &'static str,
    /// The checkout's commit, in full; empty when the source had none (an
    /// archive, as a package manager builds from).
    pub commit: &'static str,
    /// A packaged release, as opposed to a build someone made from a
    /// checkout.
    pub release: bool,
}

impl Build {
    /// This build of the workspace.
    pub const fn this() -> Build {
        Build {
            version: env!("CARGO_PKG_VERSION"),
            commit: env!("PINGPONG_BUILD_COMMIT"),
            release: !env!("PINGPONG_BUILD_RELEASE").is_empty(),
        }
    }

    /// The commit as git abbreviates it.
    pub fn short_commit(&self) -> &str {
        self.commit.get(..7).unwrap_or(self.commit)
    }

    /// Whether this build can be compared with `main`: only a checkout's.
    pub fn follows_main(&self) -> bool {
        !self.commit.is_empty()
    }

    /// "0.6.0", or "0.6.0 (4f87575)" for a build from a checkout.
    pub fn describe(&self) -> String {
        if self.release || self.commit.is_empty() {
            self.version.to_string()
        } else {
            format!("{} ({})", self.version, self.short_commit())
        }
    }
}

/// What to be told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Nothing: GitHub is asked only by "Check for Updates".
    Off,
    /// A release newer than this build.
    Releases,
    /// That, and commits on `main` newer than this build's.
    Main,
}

impl Channel {
    /// A packaged release follows releases; a build from a checkout follows
    /// `main` too, as whoever built it does.
    pub fn default_for(build: &Build) -> Channel {
        if build.follows_main() && !build.release {
            Channel::Main
        } else {
            Channel::Releases
        }
    }

    /// What a saved setting means for this build: `main` cannot be followed
    /// without a commit to compare.
    pub fn for_build(self, build: &Build) -> Channel {
        if self == Channel::Main && !build.follows_main() {
            Channel::Releases
        } else {
            self
        }
    }
}

/// Something newer than this build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Update {
    /// A release with a higher version.
    Release { version: String, url: String },
    /// `main` has this many commits the build does not.
    Commits { ahead: u32, url: String },
}

impl Update {
    /// "Ping 0.7.0 is available", "4 newer commits on main".
    pub fn headline(&self, app: &str) -> String {
        match self {
            Update::Release { version, .. } => format!("{app} {version} is available"),
            Update::Commits { ahead: 1, .. } => "1 newer commit on main".into(),
            Update::Commits { ahead, .. } => format!("{ahead} newer commits on main"),
        }
    }

    /// The release's page, or the comparison with `main`.
    pub fn url(&self) -> &str {
        match self {
            Update::Release { url, .. } | Update::Commits { url, .. } => url,
        }
    }

    /// What the button that opens [`Update::url`] says.
    pub fn link_label(&self) -> &'static str {
        match self {
            Update::Release { .. } => "Release Notes",
            Update::Commits { .. } => "See the Changes",
        }
    }

    /// How to get it, for this copy of the app: `cask` is its Homebrew cask
    /// (a copy Homebrew installed is updated by Homebrew).
    pub fn how(&self, cask: &str) -> String {
        match self {
            Update::Release { .. } if installed_by_homebrew(cask) => {
                format!("Homebrew installed this copy: run brew upgrade --cask {cask} to update.")
            }
            Update::Release { .. } => {
                "Download it from the release's page, or build it from source.".into()
            }
            Update::Commits { .. } => {
                "It was built from a checkout: pull main and build again to update.".into()
            }
        }
    }
}

/// Whether Homebrew has `cask` installed (its Caskroom has a folder for it).
pub fn installed_by_homebrew(cask: &str) -> bool {
    let prefixes = std::env::var_os("HOMEBREW_PREFIX")
        .map(PathBuf::from)
        .into_iter()
        .chain(["/opt/homebrew", "/usr/local"].map(PathBuf::from));
    cfg!(target_os = "macos")
        && prefixes
            .into_iter()
            .any(|p| p.join("Caskroom").join(cask).is_dir())
}

/// What the check knows, for the window to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// GitHub is being asked right now.
    pub checking: bool,
    pub update: Option<Update>,
    /// When GitHub last answered (Unix seconds; 0: never).
    pub checked_unix: u64,
    /// Why the last check got no answer.
    pub error: Option<String>,
}

/// What a check found, kept between launches (`update.toml` in the app's
/// data folder) so the app neither asks at every start nor forgets an update
/// it was told about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    checked_unix: u64,
    /// The build that asked: another build's answer is not this one's.
    version: String,
    commit: String,
    /// What was followed when it asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<Channel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<Update>,
}

impl Saved {
    fn path(dir: &Path) -> PathBuf {
        dir.join("update.toml")
    }

    /// The last answer, if this build asked for it.
    fn load(dir: &Path, build: &Build) -> Saved {
        std::fs::read_to_string(Saved::path(dir))
            .ok()
            .and_then(|text| toml::from_str::<Saved>(&text).ok())
            .filter(|s| s.version == build.version && s.commit == build.commit)
            .unwrap_or_default()
    }

    fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(Saved::path(dir), text) {
                    tracing::warn!(error = %e, "the update check's answer was not saved");
                }
            }
            Err(e) => tracing::warn!(error = %e, "the update check's answer was not saved"),
        }
    }

    /// What of the saved answer `channel` wants to hear.
    fn update_for(&self, channel: Channel) -> Option<Update> {
        match (&self.update, channel) {
            (_, Channel::Off) => None,
            (Some(Update::Commits { .. }), Channel::Releases) => None,
            (update, _) => update.clone(),
        }
    }

    /// Whether the saved answer covers what `channel` follows: an answer
    /// from when only releases were followed says nothing about `main`.
    fn answers(&self, channel: Channel) -> bool {
        matches!(
            (self.channel, channel),
            (_, Channel::Off)
                | (Some(Channel::Main), _)
                | (Some(Channel::Releases), Channel::Releases)
        )
    }
}

enum Ask {
    /// "Check for Updates": now, whatever the channel and the last answer.
    Now,
    Channel(Channel),
    /// A made-up answer (the UI demo).
    Pretend(Option<Update>),
}

/// The update check, running on its thread for as long as this lives.
pub struct Checker {
    asks: Sender<Ask>,
    status: Arc<Mutex<Status>>,
}

impl Checker {
    /// Start following `channel` for `build`, keeping answers in `dir`;
    /// `wake` is called (on the check's thread) whenever [`Checker::status`]
    /// has changed.
    pub fn start(
        build: Build,
        dir: PathBuf,
        channel: Channel,
        wake: impl Fn() + Send + 'static,
    ) -> Checker {
        let (asks, rx) = crossbeam_channel::unbounded();
        let status = Arc::new(Mutex::new(Status::default()));
        let shared = status.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let api = github::Api::new(&build);
                run(
                    &api,
                    build,
                    &dir,
                    channel.for_build(&build),
                    rx,
                    &shared,
                    &wake,
                )
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no update check: its thread did not start");
        }
        Checker { asks, status }
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    /// Ask GitHub now.
    pub fn check_now(&self) {
        let _ = self.asks.send(Ask::Now);
    }

    pub fn set_channel(&self, channel: Channel) {
        let _ = self.asks.send(Ask::Channel(channel));
    }

    /// Show `update` as if GitHub had said so (the UI demo; nothing is
    /// asked or saved).
    pub fn pretend(&self, update: Option<Update>) {
        let _ = self.asks.send(Ask::Pretend(update));
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The check's thread: wait until an answer is due or something is asked,
/// look, say what was found.
fn run(
    fetch: &dyn github::Fetch,
    build: Build,
    dir: &Path,
    mut channel: Channel,
    asks: Receiver<Ask>,
    status: &Mutex<Status>,
    wake: &dyn Fn(),
) {
    let mut saved = Saved::load(dir, &build);
    // Set after a failure: no retry before then.
    let mut not_before: Option<std::time::Instant> = None;
    let mut first = true;
    {
        let mut s = status.lock();
        s.update = saved.update_for(channel);
        s.checked_unix = saved.checked_unix;
    }
    if saved.update.is_some() {
        wake();
    }
    loop {
        // How long until GitHub is due a question (`None`: never).
        let due = (channel != Channel::Off).then(|| {
            let age = now_unix().saturating_sub(saved.checked_unix);
            let mut wait = if saved.answers(channel) {
                CHECK_EVERY.saturating_sub(Duration::from_secs(age))
            } else {
                Duration::ZERO
            };
            if let Some(at) = not_before {
                wait = wait.max(at.saturating_duration_since(std::time::Instant::now()));
            }
            if first {
                wait = wait.max(FIRST_CHECK_AFTER);
            }
            wait
        });
        let ask = match due {
            Some(wait) => asks.recv_timeout(wait),
            None => asks.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        first = false;
        let follow = match ask {
            Ok(Ask::Now) => match channel {
                // Turned off, and asked all the same: what this build can
                // be compared with.
                Channel::Off => Channel::default_for(&build),
                c => c,
            },
            Ok(Ask::Channel(c)) => {
                channel = c.for_build(&build);
                status.lock().update = saved.update_for(channel);
                wake();
                continue;
            }
            Ok(Ask::Pretend(update)) => {
                status.lock().update = update;
                wake();
                continue;
            }
            Err(RecvTimeoutError::Timeout) => channel,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        {
            let mut s = status.lock();
            s.checking = true;
            s.error = None;
        }
        wake();
        let found = github::look(fetch, REPOSITORY, &build, follow);
        let mut s = status.lock();
        s.checking = false;
        match found {
            Ok(update) => {
                match &update {
                    Some(u) => tracing::info!(update = u.headline("pingpong"), "update check"),
                    None => tracing::info!("update check: up to date"),
                }
                saved = Saved {
                    checked_unix: now_unix(),
                    version: build.version.to_string(),
                    commit: build.commit.to_string(),
                    channel: Some(follow),
                    update,
                };
                saved.save(dir);
                not_before = None;
                // Asked by hand while turned off: the answer is shown.
                s.update = if channel == Channel::Off {
                    saved.update.clone()
                } else {
                    saved.update_for(channel)
                };
                s.checked_unix = saved.checked_unix;
            }
            Err(e) => {
                tracing::warn!(error = e, "update check failed");
                not_before = Some(std::time::Instant::now() + RETRY_AFTER);
                s.error = Some(e);
            }
        }
        drop(s);
        wake();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const MINE: &str = "1111111111111111111111111111111111111111";

    fn build(commit: &'static str, release: bool) -> Build {
        Build {
            version: "0.6.0",
            commit,
            release,
        }
    }

    /// GitHub with one release, counting the questions.
    struct OneRelease {
        tag: &'static str,
        asked: AtomicUsize,
    }

    impl github::Fetch for OneRelease {
        fn get(&self, path: &str, _accept: &str) -> Result<(u16, String), String> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            if path.ends_with("/releases/latest") {
                Ok((200, format!(r#"{{"tag_name": "{}"}}"#, self.tag)))
            } else {
                Ok((200, MINE.to_string()))
            }
        }
    }

    /// Run the check's thread body against `fetch` for one "check now".
    fn check_once(fetch: &OneRelease, build: Build, dir: &Path, channel: Channel) -> Status {
        let (tx, rx) = crossbeam_channel::unbounded();
        let status = Mutex::new(Status::default());
        tx.send(Ask::Now).unwrap();
        drop(tx);
        run(fetch, build, dir, channel, rx, &status, &|| {});
        status.into_inner()
    }

    #[test]
    fn a_packaged_release_follows_releases_and_a_checkout_follows_main() {
        assert_eq!(Channel::default_for(&build("", false)), Channel::Releases);
        assert_eq!(Channel::default_for(&build(MINE, true)), Channel::Releases);
        assert_eq!(Channel::default_for(&build(MINE, false)), Channel::Main);
        // A setting carried over to a build with no commit to compare.
        assert_eq!(
            Channel::Main.for_build(&build("", false)),
            Channel::Releases
        );
        assert_eq!(Channel::Off.for_build(&build("", false)), Channel::Off);
    }

    #[test]
    fn a_build_says_which_commit_it_is_unless_it_is_a_release() {
        assert_eq!(build(MINE, false).describe(), "0.6.0 (1111111)");
        assert_eq!(build(MINE, true).describe(), "0.6.0");
        assert_eq!(build("", false).describe(), "0.6.0");
        assert!(build(MINE, false).follows_main());
        assert!(!build("", false).follows_main());
    }

    #[test]
    fn an_update_says_what_it_is_in_the_apps_words() {
        let release = Update::Release {
            version: "0.7.0".into(),
            url: "https://github.com/example/pingpong/releases/tag/v0.7.0".into(),
        };
        assert_eq!(release.headline("Ping"), "Ping 0.7.0 is available");
        assert_eq!(release.link_label(), "Release Notes");
        let one = Update::Commits {
            ahead: 1,
            url: String::new(),
        };
        assert_eq!(one.headline("Pong"), "1 newer commit on main");
        let four = Update::Commits {
            ahead: 4,
            url: String::new(),
        };
        assert_eq!(four.headline("Pong"), "4 newer commits on main");
        assert!(four.how("ping").contains("pull main"));
    }

    #[test]
    fn a_found_update_is_shown_and_remembered_for_the_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let github = OneRelease {
            tag: "v0.7.0",
            asked: AtomicUsize::new(0),
        };
        let status = check_once(&github, build("", false), dir.path(), Channel::Releases);
        assert!(matches!(status.update, Some(Update::Release { .. })));
        assert!(status.checked_unix > 0 && !status.checking && status.error.is_none());
        // The next launch knows without asking.
        let saved = Saved::load(dir.path(), &build("", false));
        assert_eq!(saved.update, status.update);
        assert!(saved.answers(Channel::Releases));
        // It was an answer about releases: following main asks again.
        assert!(!saved.answers(Channel::Main));
    }

    #[test]
    fn another_builds_answer_is_not_this_builds() {
        let dir = tempfile::tempdir().unwrap();
        let github = OneRelease {
            tag: "v0.7.0",
            asked: AtomicUsize::new(0),
        };
        check_once(&github, build("", false), dir.path(), Channel::Releases);
        // The update was installed: the new build starts from nothing.
        let newer = Build {
            version: "0.7.0",
            commit: "",
            release: true,
        };
        assert_eq!(Saved::load(dir.path(), &newer), Saved::default());
    }

    #[test]
    fn turned_off_nothing_is_asked_until_the_user_asks() {
        let dir = tempfile::tempdir().unwrap();
        let github = OneRelease {
            tag: "v0.7.0",
            asked: AtomicUsize::new(0),
        };
        // No "check now": the thread ends when the app does, having asked
        // nothing.
        let (tx, rx) = crossbeam_channel::unbounded::<Ask>();
        drop(tx);
        let status = Mutex::new(Status::default());
        run(
            &github,
            build("", false),
            dir.path(),
            Channel::Off,
            rx,
            &status,
            &|| {},
        );
        assert_eq!(github.asked.load(Ordering::SeqCst), 0);
        assert_eq!(status.into_inner(), Status::default());
        // Asked by hand: GitHub is asked, and the answer shown.
        let status = check_once(&github, build("", false), dir.path(), Channel::Off);
        assert_eq!(github.asked.load(Ordering::SeqCst), 1);
        assert!(status.update.is_some());
    }

    #[test]
    fn commits_on_main_are_not_said_to_someone_following_releases() {
        let saved = Saved {
            update: Some(Update::Commits {
                ahead: 3,
                url: String::new(),
            }),
            ..Saved::default()
        };
        assert_eq!(saved.update_for(Channel::Releases), None);
        assert_eq!(saved.update_for(Channel::Off), None);
        assert!(saved.update_for(Channel::Main).is_some());
    }

    #[test]
    fn a_failed_check_keeps_what_was_known_and_says_why() {
        struct Down;
        impl github::Fetch for Down {
            fn get(&self, _: &str, _: &str) -> Result<(u16, String), String> {
                Err("no route to host".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let known = Saved {
            checked_unix: 1,
            version: "0.6.0".into(),
            commit: String::new(),
            channel: Some(Channel::Releases),
            update: Some(Update::Release {
                version: "0.7.0".into(),
                url: String::new(),
            }),
        };
        known.save(dir.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(Ask::Now).unwrap();
        drop(tx);
        let status = Mutex::new(Status::default());
        run(
            &Down,
            build("", false),
            dir.path(),
            Channel::Releases,
            rx,
            &status,
            &|| {},
        );
        let status = status.into_inner();
        assert_eq!(status.error.as_deref(), Some("no route to host"));
        assert_eq!(status.update, known.update);
        assert_eq!(Saved::load(dir.path(), &build("", false)), known);
    }
}
