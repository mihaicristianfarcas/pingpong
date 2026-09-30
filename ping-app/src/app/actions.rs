//! What the menu bar's items and their keys do in the window: its pages,
//! adding a host, looking again, Check for Updates, Escape. Quit, Hide and
//! the window's own (Close, Minimize, Zoom) are the app's, in
//! `pingpong_ui::menus`.

use gpui::{prelude::*, Context, Div};
use pingpong_ui::menus;

use crate::settings::Tab;

use super::{Page, PingApp};

impl PingApp {
    /// The window's actions, on its root element.
    pub(super) fn on_actions(&self, root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(
            cx.listener(|this, _: &crate::OpenSettings, _, cx| {
                this.open_settings(Tab::General, cx)
            }),
        )
        .on_action(
            cx.listener(|this, _: &crate::ShowGeneral, _, cx| this.open_settings(Tab::General, cx)),
        )
        .on_action(
            cx.listener(|this, _: &crate::ShowVideo, _, cx| this.open_settings(Tab::Video, cx)),
        )
        .on_action(
            cx.listener(|this, _: &crate::ShowAudio, _, cx| this.open_settings(Tab::Audio, cx)),
        )
        .on_action(
            cx.listener(|this, _: &crate::ShowInput, _, cx| this.open_settings(Tab::Input, cx)),
        )
        .on_action(
            cx.listener(|this, _: &crate::ShowAgentSetup, _, cx| {
                this.open_settings(Tab::Agents, cx)
            }),
        )
        .on_action(
            cx.listener(|this, _: &menus::CheckForUpdates, _, cx| this.show_updates(true, cx)),
        )
        .on_action(cx.listener(|this, _: &crate::ShowHosts, _, cx| this.set_page(Page::Hosts, cx)))
        .on_action(
            cx.listener(|this, _: &crate::ShowAgents, _, cx| this.set_page(Page::Agents, cx)),
        )
        .on_action(cx.listener(|this, _: &crate::Refresh, _, cx| this.refresh(cx)))
        .on_action(cx.listener(|this, _: &crate::AddHost, window, cx| {
            if !this.sheet_open() {
                this.show_add_host(window, cx)
            }
        }))
        .on_action(cx.listener(|this, _: &crate::Dismiss, _, cx| this.dismiss(cx)))
    }

    /// A settings page from the menu bar or its key (not over a sheet).
    fn open_settings(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if !self.sheet_open() {
            self.set_page(Page::Settings(tab), cx)
        }
    }

    /// Open the update sheet. `ask`: GitHub is asked now (Check for
    /// Updates); otherwise only when nothing newer is known yet.
    pub fn show_updates(&mut self, ask: bool, cx: &mut Context<Self>) {
        self.menu = None;
        self.update_sheet = true;
        if ask || self.update_status.update.is_none() {
            self.updates.check_now();
        }
        cx.notify();
    }
}
