//! The Home screen (placeholder).

/// Placeholder until the Home screen is ported to GPUI.
pub struct HomeView {
    #[allow(dead_code)] // read once the screen has content
    app: gpui::WeakEntity<crate::app::ZeroClipApp>,
}

impl HomeView {
    pub fn new(app: gpui::WeakEntity<crate::app::ZeroClipApp>) -> Self {
        Self { app }
    }
}

impl gpui::Render for HomeView {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().p_4().child("Home")
    }
}
