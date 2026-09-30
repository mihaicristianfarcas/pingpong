//! Notifications about agent sessions, for when you are not looking at
//! them: the agent asks for your go-ahead (Allow and Don't are on the
//! notification itself), waits for you at a sign-in or administrator
//! prompt, is done, or stopped. Clicking one opens its session.
//!
//! On a Mac, through the system's notification center (Ping.app asks for
//! permission the first time a session opens). Elsewhere, nothing yet.

use std::collections::HashMap;

use crate::app::Waker;

/// What the user did with a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Tap {
    /// Clicked it: show the session.
    Open(u64),
    /// Allow (true) or Don't (false) on a request for a go-ahead.
    Answer(u64, bool),
}

/// A notification a session wants shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A step waits for the user's yes.
    Ask,
    /// The agent waits for the user at the host (a secure screen).
    Waiting,
    Done,
    Stopped,
}

pub struct Notifier {
    taps: crossbeam_channel::Receiver<Tap>,
    #[cfg(target_os = "macos")]
    center: Option<mac::Center>,
    /// The request for a go-ahead shown for each session.
    asks: HashMap<u64, String>,
    authorized: bool,
    next: u64,
}

impl Notifier {
    pub fn new(waker: Waker) -> Notifier {
        let (tx, taps) = crossbeam_channel::unbounded();
        let sink = move |tap: Tap| {
            tracing::info!(?tap, "notification clicked");
            let _ = tx.send(tap);
            waker.wake();
        };
        #[cfg(target_os = "macos")]
        let center = mac::Center::open(sink);
        #[cfg(not(target_os = "macos"))]
        drop(sink);
        Notifier {
            taps,
            #[cfg(target_os = "macos")]
            center,
            asks: HashMap::new(),
            authorized: false,
            next: 0,
        }
    }

    /// Ask the system for permission to notify (once; the system asks the
    /// user the first time only).
    // The early return only has something to skip on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(clippy::needless_return))]
    pub fn authorize(&mut self) {
        if std::mem::replace(&mut self.authorized, true) {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(c) = &self.center {
            c.authorize();
        }
    }

    /// Show `notice` for session `chat`.
    pub fn post(&mut self, chat: u64, notice: &Notice) {
        self.next += 1;
        let id = format!("ping-chat-{chat}-{}", self.next);
        tracing::info!(chat, kind = ?notice.kind, "notification");
        if notice.kind == NoticeKind::Ask {
            if let Some(old) = self.asks.insert(chat, id.clone()) {
                self.remove(&old);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(c) = &self.center {
            c.post(
                &id,
                &format!("ping-chat-{chat}"),
                &notice.title,
                &notice.body,
                notice.kind == NoticeKind::Ask,
            );
        }
    }

    /// The request for a go-ahead in session `chat` was answered (or went):
    /// its notification goes.
    pub fn answered(&mut self, chat: u64) {
        if let Some(id) = self.asks.remove(&chat) {
            self.remove(&id);
        }
    }

    pub fn has_ask(&self, chat: u64) -> bool {
        self.asks.contains_key(&chat)
    }

    fn remove(&self, _id: &str) {
        #[cfg(target_os = "macos")]
        if let Some(c) = &self.center {
            c.remove(_id);
        }
    }

    /// What the user did with notifications since last asked.
    pub fn taps(&self) -> Vec<Tap> {
        self.taps.try_iter().collect()
    }
}

/// The session a notification's identifier names.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn chat_of(id: &str) -> Option<u64> {
    id.strip_prefix("ping-chat-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::OnceLock;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{define_class, msg_send, AllocAnyThread};
    use objc2_foundation::{ns_string, NSArray, NSBundle, NSError, NSSet, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    use super::Tap;

    type Sink = Box<dyn Fn(Tap) + Send + Sync>;
    static SINK: OnceLock<Sink> = OnceLock::new();

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "PingNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // Shown even with Ping in front: it only posts about sessions
            // not on screen.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                done.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                done: &block2::DynBlock<dyn Fn()>,
            ) {
                let id = response.notification().request().identifier().to_string();
                let action = response.actionIdentifier().to_string();
                if let (Some(chat), Some(sink)) = (super::chat_of(&id), SINK.get()) {
                    sink(match action.as_str() {
                        "allow" => Tap::Answer(chat, true),
                        "deny" => Tap::Answer(chat, false),
                        _ => Tap::Open(chat),
                    });
                }
                done.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Delegate> {
            unsafe { msg_send![Delegate::alloc(), init] }
        }
    }

    pub struct Center {
        center: Retained<UNUserNotificationCenter>,
        _delegate: Retained<Delegate>,
    }

    impl Center {
        /// None outside an app bundle (a binary run from a terminal has no
        /// notification center of its own).
        pub fn open(sink: impl Fn(Tap) + Send + Sync + 'static) -> Option<Center> {
            if NSBundle::mainBundle().bundleIdentifier().is_none() {
                tracing::info!("not an app bundle: no notifications");
                return None;
            }
            let _ = SINK.set(Box::new(sink));
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = Delegate::new();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // A request for a go-ahead is answered on the notification.
            let allow = UNNotificationAction::actionWithIdentifier_title_options(
                ns_string!("allow"),
                ns_string!("Allow"),
                UNNotificationActionOptions::empty(),
            );
            let deny = UNNotificationAction::actionWithIdentifier_title_options(
                ns_string!("deny"),
                ns_string!("Don't"),
                UNNotificationActionOptions::Destructive,
            );
            let ask =
                UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                    ns_string!("ask"),
                    &NSArray::from_retained_slice(&[allow, deny]),
                    &NSArray::new(),
                    UNNotificationCategoryOptions::empty(),
                );
            center.setNotificationCategories(&NSSet::from_retained_slice(&[ask]));
            Some(Center {
                center,
                _delegate: delegate,
            })
        }

        pub fn authorize(&self) {
            let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
                let error = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
                tracing::info!(granted = granted.as_bool(), error, "notifications");
            });
            self.center
                .requestAuthorizationWithOptions_completionHandler(
                    UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                    &done,
                );
        }

        pub fn post(&self, id: &str, thread: &str, title: &str, body: &str, ask: bool) {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(title));
            content.setBody(&NSString::from_str(body));
            content.setSound(Some(&UNNotificationSound::defaultSound()));
            content.setThreadIdentifier(&NSString::from_str(thread));
            if ask {
                content.setCategoryIdentifier(ns_string!("ask"));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(id),
                &content,
                None,
            );
            let done = RcBlock::new(|error: *mut NSError| {
                if let Some(e) = unsafe { error.as_ref() } {
                    tracing::warn!(error = %e.localizedDescription(), "a notification was not shown");
                }
            });
            self.center
                .addNotificationRequest_withCompletionHandler(&request, Some(&done));
        }

        pub fn remove(&self, id: &str) {
            let ids = NSArray::from_retained_slice(&[NSString::from_str(id)]);
            self.center
                .removeDeliveredNotificationsWithIdentifiers(&ids);
            self.center
                .removePendingNotificationRequestsWithIdentifiers(&ids);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn identifiers_name_their_session() {
        assert_eq!(super::chat_of("ping-chat-12-3"), Some(12));
        assert_eq!(super::chat_of("other-12"), None);
    }
}
