//! PING_UI_DEMO: made-up sessions, for checks and screenshots (see docs/ui.md).

use std::time::{Duration, Instant};

use gpui::{Context, Window};
use ping_agent::computer::{LinkStatus, PlanStatus, PlanStep};
use ping_agent::providers::AgentSettings;

use crate::app::{PingApp, Waker};

use super::{Act, Chat, Item, Turn, STRIP_PIXELS};

impl Chat {
    /// PING_UI_DEMO=agents=sample: a session with a made-up transcript.
    pub fn sample(
        id: u64,
        waker: Waker,
        live: bool,
        window: &mut Window,
        cx: &mut Context<PingApp>,
    ) -> Chat {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut c = Chat::new(
            id,
            "GAMING-PC",
            AgentSettings::default(),
            None,
            tx,
            rx,
            waker,
            window,
            cx,
        );
        c.title = "Clean up the Downloads folder".into();
        c.items = vec![
            Item::You(
                "How much space is the Downloads folder using, and what's the \
                    biggest thing in it?"
                    .into(),
            ),
            Item::Actions {
                list: vec![
                    Act {
                        text: "connect GAMING-PC".into(),
                        ok: true,
                        detail: "1280x800, Windows".into(),
                    },
                    Act {
                        text: "key super+e".into(),
                        ok: true,
                        detail: "File Explorer".into(),
                    },
                    Act {
                        text: "left_click (96, 412)".into(),
                        ok: true,
                        detail: String::new(),
                    },
                    Act {
                        text: "key alt+Return".into(),
                        ok: false,
                        detail: "Properties took a moment".into(),
                    },
                ],
                open: false,
            },
            Item::Reply(
                "**Downloads** holds **18.4 GB** in 212 files. The biggest \
                    items:\n\n| File | Size |\n|---|---|\n| `ubuntu-24.04-desktop.iso` | 5.8 \
                    GB |\n| `cyberpunk-patch-2.3.zip` | 3.1 GB |\n| `OBS recordings/` | 2.6 GB \
                    |\n\nWant me to move the ISO and the patch to the recycle bin?"
                    .into(),
            ),
            Item::You("Yes, both. Keep the recordings.".into()),
            Item::Actions {
                list: vec![
                    Act {
                        text: "left_click (402, 188)".into(),
                        ok: true,
                        detail: String::new(),
                    },
                    Act {
                        text: "key ctrl+left_click (402, 214)".into(),
                        ok: true,
                        detail: String::new(),
                    },
                    Act {
                        text: "key Delete".into(),
                        ok: true,
                        detail: "moved to the Recycle Bin".into(),
                    },
                ],
                open: live,
            },
        ];
        if !live {
            c.items.push(Item::Reply(
                "Done: both are in the **Recycle Bin**, 8.9 GB freed. \
                    The recordings are where they were.\n\n- Empty the bin to get the space \
                    back for good\n- Or tell me to, and I will"
                    .into(),
            ));
        } else {
            let (answers, _) = crossbeam_channel::bounded(1);
            c.turn = Some(Turn {
                started: Instant::now() - Duration::from_secs(42),
                paused: false,
                question: None,
                answers,
                status: Some("Working…".into()),
            });
        }
        c.actions = 7;
        c.turn_actions = 3;
        c.tokens = (61_204, 2_310, None);
        c.connected = true;
        c.link = Some(LinkStatus {
            rtt_ms: 9.6,
            loss_pct: 0.0,
            agent: Some(pingpong_proto::control::AgentState {
                flags: 0,
                watchers: 0,
            }),
        });
        c.plan = vec![
            PlanStep {
                text: "Open the Downloads folder".into(),
                status: PlanStatus::Done,
            },
            PlanStep {
                text: "Sort by size, find the biggest".into(),
                status: PlanStatus::Done,
            },
            PlanStep {
                text: "Move the ISO and the patch to the bin".into(),
                status: if live {
                    PlanStatus::InProgress
                } else {
                    PlanStatus::Done
                },
            },
            PlanStep {
                text: "Tell you what was freed".into(),
                status: if live {
                    PlanStatus::Pending
                } else {
                    PlanStatus::Done
                },
            },
        ];
        // The steps' pictures: the screen given (the same one for each).
        if let Some(bytes) = std::env::var("PING_UI_DEMO_SCREEN")
            .ok()
            .and_then(|p| std::fs::read(p).ok())
        {
            let thumb = ping_agent::computer::shrink_png(&bytes, STRIP_PIXELS).map(|s| s.png);
            let points = [
                None,
                None,
                Some((0.075, 0.515)),
                None,
                Some((0.314, 0.235)),
                Some((0.314, 0.268)),
                None,
            ];
            let captions = [
                "connect GAMING-PC",
                "key super+e",
                "left_click (96, 412)",
                "key alt+Return",
                "left_click (402, 188)",
                "key ctrl+left_click (402, 214)",
                "key Delete",
            ];
            for (caption, point) in captions.iter().zip(points) {
                c.add_step(caption, true, bytes.clone(), thumb.clone(), point);
            }
            for (i, step) in c.steps.iter_mut().enumerate() {
                step.at = Instant::now() - Duration::from_secs(4 + 9 * (6 - i as u64));
            }
        }
        c
    }
}
