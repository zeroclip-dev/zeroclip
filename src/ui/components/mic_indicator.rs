//! The "Mic live" / "Mic muted" indicator, so the owner can see push-to-talk working while playing.

use eframe::egui::{self, Color32, Response, Sense, Ui, Widget};
use zc_core::control::MicState;

/// A dot and a label for the microphone's state. Off draws nothing, so the caller only adds its
/// separator when there is something to show.
#[derive(Clone, Copy, Debug)]
pub struct MicIndicator(pub MicState);

impl Widget for MicIndicator {
    fn ui(self, ui: &mut Ui) -> Response {
        let live = match self.0 {
            MicState::Off => return ui.allocate_response(egui::Vec2::ZERO, Sense::hover()),
            MicState::Live => true,
            MicState::Muted => false,
        };
        let dot = if live { Color32::LIGHT_RED } else { Color32::GRAY };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.5, dot);
            if live {
                ui.label("Mic live");
            } else {
                ui.weak("Mic muted");
            }
        })
        .response
    }
}
