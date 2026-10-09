//! The account's friends, and what they are doing: Xbox Live's people hub
//! (`peoplehub.xboxlive.com`), as Greenlight lists the friends who are
//! online (`xbox-webapi`'s `people.js`, its sidebar's `frienditem.tsx`).
//!
//! The service allows 30 requests in five minutes, so the list is asked
//! for when it is looked at, not on a timer.

use serde::Deserialize;

use crate::auth::{Auth, AuthError};
use crate::http::Body;

/// A friend, as the account sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Friend {
    pub xuid: String,
    pub gamertag: String,
    /// The name to show: the gamertag, or the real name the friend shares.
    pub name: String,
    pub online: bool,
    /// What they are doing: the game they play, else what Xbox Live says
    /// ("Online", "Last seen 2h ago: Xbox App").
    pub activity: String,
}

/// The account's friends, those online first, each group by name.
pub fn friends(auth: &mut Auth) -> Result<Vec<Friend>, AuthError> {
    let authorization = auth.web_token()?.authorization();
    let answer = auth.http.ok(
        "GET",
        "peoplehub.xboxlive.com",
        "/users/me/people/social/decoration/preferredcolor,detail,multiplayersummary,presencedetail",
        &[
            ("Authorization", &authorization),
            ("x-xbl-contract-version", "3"),
            ("Accept-Language", "en-US"),
        ],
        Body::None,
    )?;
    parse_friends(&answer.body)
}

pub fn parse_friends(body: &str) -> Result<Vec<Friend>, AuthError> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default)]
        people: Vec<Person>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Person {
        xuid: String,
        #[serde(default)]
        gamertag: String,
        #[serde(default)]
        display_name: String,
        #[serde(default)]
        presence_state: String,
        #[serde(default)]
        presence_text: String,
        #[serde(default)]
        presence_details: Vec<Detail>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Detail {
        #[serde(default)]
        is_game: bool,
        #[serde(default)]
        is_primary: bool,
        #[serde(default)]
        presence_text: String,
    }
    let a: Answer = serde_json::from_str(body)
        .map_err(|e| AuthError::Failed(format!("The friends list could not be read: {e}")))?;
    let mut friends: Vec<Friend> = a
        .people
        .into_iter()
        .map(|p| {
            let game = p
                .presence_details
                .iter()
                .find(|d| d.is_game && d.is_primary && !d.presence_text.is_empty())
                .map(|d| d.presence_text.clone());
            Friend {
                name: if p.display_name.is_empty() {
                    p.gamertag.clone()
                } else {
                    p.display_name
                },
                gamertag: p.gamertag,
                xuid: p.xuid,
                online: p.presence_state == "Online",
                activity: game.unwrap_or(p.presence_text),
            }
        })
        .collect();
    friends.sort_by_key(|f| (!f.online, f.name.to_lowercase()));
    Ok(friends)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friends_online_come_first_with_the_game_they_play() {
        let body = r#"{"people":[
            {"xuid":"1","gamertag":"Zed","displayName":"","presenceState":"Offline","presenceText":"Last seen 2h ago: Xbox App","presenceDetails":[]},
            {"xuid":"2","gamertag":"amy","displayName":"Amy","presenceState":"Online","presenceText":"Online",
             "presenceDetails":[{"IsBroadcasting":false,"Device":"Scarlett","PresenceText":"Home","State":"Active","TitleId":"1","IsGame":false,"IsPrimary":false},
                                {"IsBroadcasting":false,"Device":"Scarlett","PresenceText":"Forza Horizon 5","State":"Active","TitleId":"2","IsGame":true,"IsPrimary":true}]},
            {"xuid":"3","gamertag":"Bob","displayName":"Bob","presenceState":"Online","presenceText":"Online","presenceDetails":[]}
        ],"recommendationSummary":null}"#;
        let f = parse_friends(body).unwrap();
        let names: Vec<&str> = f.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Amy", "Bob", "Zed"]);
        assert_eq!(f[0].activity, "Forza Horizon 5");
        assert_eq!(f[1].activity, "Online");
        assert!(!f[2].online);
        assert_eq!(f[2].activity, "Last seen 2h ago: Xbox App");
    }

    #[test]
    fn an_unreadable_list_is_an_error() {
        assert!(parse_friends("<html>").is_err());
        assert_eq!(parse_friends("{}").unwrap(), vec![]);
    }
}
