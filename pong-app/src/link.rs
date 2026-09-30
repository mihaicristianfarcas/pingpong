//! The host as this app knows it: the worker's latest word on it, for
//! whoever is looking. That is the window while one is open and the tray
//! icon all the time, so it belongs to neither: the app keeps it, and both
//! observe it.

use crossbeam_channel::Sender;
use futures::StreamExt;
use gpui::{App, AppContext, Entity, EventEmitter};

use crate::worker::{self, Cmd, Done, Msg, Snapshot};

pub struct Link {
    pub snap: Snapshot,
    cmds: Sender<Cmd>,
}

/// What the host answered to something asked of it.
impl EventEmitter<Done> for Link {}

impl Link {
    /// Start the worker and keep what it says.
    pub fn start(cx: &mut App) -> Entity<Link> {
        let (news, mut woken) = futures::channel::mpsc::unbounded::<()>();
        let (cmds, msgs) = worker::spawn(move || {
            let _ = news.unbounded_send(());
        });
        let link = cx.new(|_| Link {
            snap: Snapshot::default(),
            cmds,
        });
        let weak = link.downgrade();
        cx.spawn(async move |cx| {
            while woken.next().await.is_some() {
                while woken.try_recv().is_ok() {}
                let taken = weak.update(cx, |link, cx| {
                    while let Ok(msg) = msgs.try_recv() {
                        match msg {
                            Msg::Snapshot(snap) => {
                                link.snap = *snap;
                                crate::app::pretend(&mut link.snap);
                                cx.notify();
                            }
                            Msg::Done(done) => cx.emit(done),
                        }
                    }
                });
                if taken.is_err() {
                    break;
                }
            }
        })
        .detach();
        link
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    /// For a window to ask the host things itself.
    pub fn sender(&self) -> Sender<Cmd> {
        self.cmds.clone()
    }
}
