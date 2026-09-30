//! What GitHub's public API says about the repository: its latest release,
//! and how far `main` is ahead of a commit. Unauthenticated (60 requests an
//! hour for an address; a check makes two, or three when `main` has moved).
//!
//! The answers are read as untrusted input: the fields that matter, bounded,
//! and anything unexpected is an error or "nothing to say", never a panic.

use std::time::Duration;

use serde::Deserialize;

use crate::version::Version;
use crate::{Build, Channel, Update};

/// A release's page and a comparison's are opened in the browser: only
/// links into the repository itself are passed on.
const SITE: &str = "https://github.com/";

/// One GET: the status and the body, or why nothing came back.
pub(crate) trait Fetch {
    fn get(&self, path: &str, accept: &str) -> Result<(u16, String), String>;
}

/// GitHub's API over HTTPS.
pub(crate) struct Api {
    base: String,
    http: ureq::Agent,
    user_agent: String,
}

/// The longest answer read. A comparison lists up to 300 changed files with
/// their patches; only its counts are used.
const MAX_BODY: u64 = 16 * 1024 * 1024;

impl Api {
    /// `PINGPONG_UPDATE_API` points it elsewhere (tests).
    pub(crate) fn new(build: &Build) -> Api {
        let base = std::env::var("PINGPONG_UPDATE_API")
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "https://api.github.com".into());
        let http: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(8)))
            .http_status_as_error(false)
            .build()
            .into();
        Api {
            base: base.trim_end_matches('/').to_string(),
            http,
            // GitHub refuses requests without one.
            user_agent: format!("pingpong/{}", build.version),
        }
    }
}

impl Fetch for Api {
    fn get(&self, path: &str, accept: &str) -> Result<(u16, String), String> {
        let mut response = self
            .http
            .get(format!("{}{path}", self.base))
            .header("User-Agent", &self.user_agent)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok((status, body))
    }
}

/// `owner/name`, from the repository's address in `Cargo.toml`.
pub(crate) fn slug(repository: &str) -> Option<&str> {
    let slug = repository
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .strip_prefix(SITE)?;
    let (owner, name) = slug.split_once('/')?;
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (plain(owner) && plain(name)).then_some(slug)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
struct Comparison {
    #[serde(default)]
    ahead_by: u32,
}

/// A commit as git names it in full: 40 hexadecimal digits (64 with SHA-256).
fn is_commit(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn failed(what: &str, status: u16) -> String {
    match status {
        // The hourly allowance for this address is used up.
        403 | 429 => "GitHub is not taking more requests from this network for now".into(),
        _ => format!("GitHub answered {status} for {what}"),
    }
}

/// The latest release, if it is newer than this build.
fn newer_release(fetch: &dyn Fetch, slug: &str, build: &Build) -> Result<Option<Update>, String> {
    let (status, body) = fetch.get(
        &format!("/repos/{slug}/releases/latest"),
        "application/vnd.github+json",
    )?;
    match status {
        200 => {}
        // No release has been published yet.
        404 => return Ok(None),
        other => return Err(failed("the latest release", other)),
    }
    let release: Release =
        serde_json::from_str(&body).map_err(|e| format!("the latest release: {e}"))?;
    let (Some(latest), Some(mine)) = (
        Version::parse(&release.tag_name),
        Version::parse(build.version),
    ) else {
        // A tag that is not a version is not a release of this program.
        return Ok(None);
    };
    if latest <= mine {
        return Ok(None);
    }
    let url = if release.html_url.starts_with(&format!("{SITE}{slug}/")) {
        release.html_url
    } else {
        format!("{SITE}{slug}/releases/latest")
    };
    Ok(Some(Update::Release {
        version: latest.to_string(),
        url,
    }))
}

/// How far `main` is ahead of this build's commit, if it is.
fn newer_commits(fetch: &dyn Fetch, slug: &str, build: &Build) -> Result<Option<Update>, String> {
    // Just the commit's name: a comparison is only asked for when main is
    // somewhere else than this build.
    let (status, body) = fetch.get(
        &format!("/repos/{slug}/commits/main"),
        "application/vnd.github.sha",
    )?;
    if status != 200 {
        return Err(failed("the main branch", status));
    }
    let main = body.trim();
    if !is_commit(main) {
        return Err("GitHub's answer for the main branch is not a commit".into());
    }
    if main.eq_ignore_ascii_case(build.commit) {
        return Ok(None);
    }
    let (status, body) = fetch.get(
        &format!("/repos/{slug}/compare/{}...{main}?per_page=1", build.commit),
        "application/vnd.github+json",
    )?;
    match status {
        200 => {}
        // A commit GitHub has never seen: a local one, ahead of main or
        // beside it. Nothing can be said about it.
        404 => return Ok(None),
        other => return Err(failed("the comparison with main", other)),
    }
    let comparison: Comparison =
        serde_json::from_str(&body).map_err(|e| format!("the comparison with main: {e}"))?;
    if comparison.ahead_by == 0 {
        return Ok(None);
    }
    Ok(Some(Update::Commits {
        ahead: comparison.ahead_by,
        url: format!("{SITE}{slug}/compare/{}...main", build.short_commit()),
    }))
}

/// One check: a newer release first; then, for a build from a checkout that
/// follows `main`, newer commits there.
pub(crate) fn look(
    fetch: &dyn Fetch,
    repository: &str,
    build: &Build,
    channel: Channel,
) -> Result<Option<Update>, String> {
    let slug = slug(repository).ok_or("this build does not know its repository")?;
    if let Some(update) = newer_release(fetch, slug, build)? {
        return Ok(Some(update));
    }
    if channel == Channel::Main && is_commit(build.commit) {
        return newer_commits(fetch, slug, build);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const REPO: &str = "https://github.com/example/pingpong";
    const MINE: &str = "1111111111111111111111111111111111111111";
    const MAIN: &str = "2222222222222222222222222222222222222222";

    /// Answers by path, and remembers what was asked.
    struct Fake {
        answers: Vec<(&'static str, u16, String)>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(answers: &[(&'static str, u16, &str)]) -> Fake {
            Fake {
                answers: answers
                    .iter()
                    .map(|(p, s, b)| (*p, *s, b.to_string()))
                    .collect(),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Fetch for Fake {
        fn get(&self, path: &str, _accept: &str) -> Result<(u16, String), String> {
            self.asked.borrow_mut().push(path.to_string());
            self.answers
                .iter()
                .find(|(p, _, _)| path.contains(p))
                .map(|(_, status, body)| (*status, body.clone()))
                .ok_or_else(|| format!("no answer for {path}"))
        }
    }

    fn build(version: &'static str, commit: &'static str) -> Build {
        Build {
            version,
            commit,
            release: false,
        }
    }

    #[test]
    fn a_newer_release_is_an_update_with_its_own_page() {
        let fake = Fake::new(&[(
            "releases/latest",
            200,
            r#"{"tag_name": "v0.7.0", "html_url": "https://github.com/example/pingpong/releases/tag/v0.7.0", "assets": []}"#,
        )]);
        let found = look(&fake, REPO, &build("0.6.0", ""), Channel::Releases);
        assert_eq!(
            found,
            Ok(Some(Update::Release {
                version: "0.7.0".into(),
                url: "https://github.com/example/pingpong/releases/tag/v0.7.0".into(),
            }))
        );
    }

    #[test]
    fn the_release_this_build_is_or_an_older_one_is_not_an_update() {
        for tag in ["v0.6.0", "0.5.9", "v0.6.0-rc.1"] {
            let body = format!(r#"{{"tag_name": "{tag}", "html_url": ""}}"#);
            let fake = Fake::new(&[("releases/latest", 200, &body)]);
            assert_eq!(
                look(&fake, REPO, &build("0.6.0", ""), Channel::Releases),
                Ok(None),
                "{tag}"
            );
        }
    }

    #[test]
    fn a_repository_without_releases_has_nothing_to_offer() {
        let fake = Fake::new(&[("releases/latest", 404, r#"{"message": "Not Found"}"#)]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", ""), Channel::Releases),
            Ok(None)
        );
    }

    #[test]
    fn a_link_that_leaves_the_repository_is_replaced() {
        let fake = Fake::new(&[(
            "releases/latest",
            200,
            r#"{"tag_name": "v9.0.0", "html_url": "https://example.com/get-it-here"}"#,
        )]);
        let Ok(Some(Update::Release { url, .. })) =
            look(&fake, REPO, &build("0.6.0", ""), Channel::Releases)
        else {
            panic!("a release");
        };
        assert_eq!(url, "https://github.com/example/pingpong/releases/latest");
    }

    #[test]
    fn main_is_only_looked_at_by_a_checkout_that_follows_it() {
        let none = ("releases/latest", 404, "");
        // Following releases: main is never asked about.
        let fake = Fake::new(&[none]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Releases),
            Ok(None)
        );
        assert_eq!(fake.asked.borrow().len(), 1);
        // No commit to compare (a build from a source archive).
        let fake = Fake::new(&[none]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", ""), Channel::Main),
            Ok(None)
        );
        assert_eq!(fake.asked.borrow().len(), 1);
    }

    #[test]
    fn commits_on_main_this_build_lacks_are_an_update() {
        let fake = Fake::new(&[
            ("releases/latest", 404, ""),
            ("commits/main", 200, MAIN),
            (
                "compare/",
                200,
                r#"{"status": "ahead", "ahead_by": 4, "behind_by": 0, "commits": [], "files": []}"#,
            ),
        ]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Main),
            Ok(Some(Update::Commits {
                ahead: 4,
                url: "https://github.com/example/pingpong/compare/1111111...main".into(),
            }))
        );
        assert!(fake.asked.borrow()[2].contains(&format!("{MINE}...{MAIN}")));
    }

    #[test]
    fn a_build_of_main_itself_asks_for_no_comparison() {
        let fake = Fake::new(&[("releases/latest", 404, ""), ("commits/main", 200, MINE)]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Main),
            Ok(None)
        );
        assert_eq!(fake.asked.borrow().len(), 2);
    }

    #[test]
    fn a_checkout_ahead_of_main_or_unknown_to_github_is_not_behind() {
        // Ahead of main: main has nothing this build lacks.
        let fake = Fake::new(&[
            ("releases/latest", 404, ""),
            ("commits/main", 200, MAIN),
            (
                "compare/",
                200,
                r#"{"status": "behind", "ahead_by": 0, "behind_by": 3}"#,
            ),
        ]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Main),
            Ok(None)
        );
        // A local commit that was never pushed.
        let fake = Fake::new(&[
            ("releases/latest", 404, ""),
            ("commits/main", 200, MAIN),
            ("compare/", 404, r#"{"message": "Not Found"}"#),
        ]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Main),
            Ok(None)
        );
    }

    #[test]
    fn a_newer_release_is_said_before_commits_on_main() {
        let fake = Fake::new(&[
            ("releases/latest", 200, r#"{"tag_name": "v0.7.0"}"#),
            ("commits/main", 200, MAIN),
        ]);
        assert!(matches!(
            look(&fake, REPO, &build("0.6.0", MINE), Channel::Main),
            Ok(Some(Update::Release { .. }))
        ));
        assert_eq!(fake.asked.borrow().len(), 1);
    }

    #[test]
    fn answers_that_are_not_what_was_asked_for_are_errors_not_panics() {
        for (path, status, body) in [
            ("releases/latest", 200, "<html>a captive portal</html>"),
            ("releases/latest", 200, r#"{"tag_name": 7}"#),
            (
                "releases/latest",
                403,
                r#"{"message": "API rate limit exceeded"}"#,
            ),
            ("releases/latest", 500, ""),
        ] {
            let fake = Fake::new(&[(path, status, body)]);
            assert!(
                look(&fake, REPO, &build("0.6.0", ""), Channel::Releases).is_err(),
                "{status} {body}"
            );
        }
        // Main's commit must be a commit: it goes into the next request.
        for body in ["", "main", "../../../etc", &"z".repeat(40)] {
            let fake = Fake::new(&[("releases/latest", 404, ""), ("commits/main", 200, body)]);
            assert!(
                look(&fake, REPO, &build("0.6.0", MINE), Channel::Main).is_err(),
                "{body:?}"
            );
            assert_eq!(fake.asked.borrow().len(), 2);
        }
        // A tag that is not a version is no release of this program.
        let fake = Fake::new(&[("releases/latest", 200, r#"{"tag_name": "nightly"}"#)]);
        assert_eq!(
            look(&fake, REPO, &build("0.6.0", ""), Channel::Releases),
            Ok(None)
        );
    }

    #[test]
    fn the_repository_is_named_by_its_github_address() {
        assert_eq!(
            slug("https://github.com/example/pingpong"),
            Some("example/pingpong")
        );
        assert_eq!(
            slug("https://github.com/example/pingpong.git"),
            Some("example/pingpong")
        );
        assert_eq!(
            slug("https://github.com/example/pingpong/"),
            Some("example/pingpong")
        );
        assert_eq!(slug("https://example.com/example/pingpong"), None);
        assert_eq!(slug("https://github.com/example"), None);
        assert_eq!(slug("https://github.com/example/ping pong"), None);
        assert_eq!(slug("https://github.com/a/b/c"), None);
    }
}
