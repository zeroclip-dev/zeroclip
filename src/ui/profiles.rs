//! The Profiles screen (placeholder).

/// Placeholder until the Profiles screen is ported to GPUI.
pub struct ProfilesView {
    #[allow(dead_code)] // read once the screen has content
    app: gpui::WeakEntity<crate::app::ZeroClipApp>,
}

impl ProfilesView {
    pub fn new(app: gpui::WeakEntity<crate::app::ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl gpui::Render for ProfilesView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().p_4().child("Profiles")
    }
}
