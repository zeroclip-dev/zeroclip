//! Tab -> view dispatch. To add or replace a screen: implement `Render` for a view in
//! `src/ui/<name>.rs` with `fn new(app: WeakEntity<ZeroClipApp>) -> Self`, keep it in the
//! matching `Screens` field, and nothing else (`ui/mod.rs` never changes).

use gpui::{AnyView, App, AppContext, Entity, WeakEntity};

use crate::app::{Tab, ZeroClipApp};
use crate::ui::{
    clips::ClipsView, editor::EditorView, home::HomeView, profiles::ProfilesView,
    settings::SettingsView, video_player::VideoPlayerView,
};

/// One live view per tab.
pub struct Screens {
    home: Entity<HomeView>,
    clips: Entity<ClipsView>,
    profiles: Entity<ProfilesView>,
    editor: Entity<EditorView>,
    settings: Entity<SettingsView>,
    player: Entity<VideoPlayerView>,
}

impl Screens {
    pub fn new(app: WeakEntity<ZeroClipApp>, cx: &mut App) -> Self {
        Self {
            home: cx.new(|_| HomeView::new(app.clone())),
            clips: cx.new(|_| ClipsView::new(app.clone())),
            profiles: cx.new(|cx| ProfilesView::new(app.clone(), cx)),
            editor: cx.new(|_| EditorView::new(app.clone())),
            settings: cx.new(|_| SettingsView::new(app.clone())),
            player: cx.new(|_| VideoPlayerView::new(app)),
        }
    }

    /// The Editor view, so other screens can open a clip in it.
    pub fn editor(&self) -> &Entity<EditorView> {
        &self.editor
    }

    /// The Clips view, so the clip menu can start a rename there.
    pub fn clips(&self) -> &Entity<ClipsView> {
        &self.clips
    }

    /// The video player, so other screens can play a clip in it.
    pub fn player(&self) -> &Entity<VideoPlayerView> {
        &self.player
    }

    /// The view shown for `tab`.
    pub fn view(&self, tab: Tab) -> AnyView {
        match tab {
            Tab::Home => self.home.clone().into(),
            Tab::Clips => self.clips.clone().into(),
            Tab::Profiles => self.profiles.clone().into(),
            Tab::Editor => self.editor.clone().into(),
            Tab::Settings => self.settings.clone().into(),
            Tab::Player => self.player.clone().into(),
        }
    }
}
