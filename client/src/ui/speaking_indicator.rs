//! Floating overlay listing currently-speaking peers.
//!
//! Defers writes through `pending_*` queues so the render path can hold a
//! read-only borrow on the voice manager. `apply_pending` is called from
//! `lib.rs` once the read borrow is released.

use crate::voice::{SpeakingIndicatorSettings, VoiceManager};
use nexus::imgui::{Condition, MouseButton, Slider, Ui, Window, WindowFlags, WindowHoveredFlags};

/// Frames the live position must hold steady before being persisted to
/// disk. Roughly half a second at 60 FPS — long enough to swallow a
/// drag, short enough to feel responsive when the user lets go.
const POSITION_STABLE_FRAMES: u32 = 30;

/// Popup id for the right-click context menu. Underscore-prefixed so it
/// doesn't collide with any user-visible label.
const CONTEXT_POPUP_ID: &str = "##vloxximity_speaking_ctx";

pub struct SpeakingIndicator {
    pending_mutes: Vec<(String, bool)>,
    pending_save_position: Option<[f32; 2]>,
    pending_settings: Option<SpeakingIndicatorSettings>,
    last_observed_pos: Option<[f32; 2]>,
    stable_frames: u32,
}

impl SpeakingIndicator {
    pub fn new() -> Self {
        Self {
            pending_mutes: Vec::new(),
            pending_save_position: None,
            pending_settings: None,
            last_observed_pos: None,
            stable_frames: 0,
        }
    }

    pub fn render(&mut self, ui: &Ui, voice_manager: &VoiceManager) {
        let ind = voice_manager.settings().speaking_indicator.clone();
        if !ind.enabled {
            return;
        }

        let peers = voice_manager.get_peers();
        let speaking_peers: Vec<_> = peers.into_iter().filter(|p| p.is_speaking).collect();
        if speaking_peers.is_empty() && !ind.show_when_silent {
            return;
        }

        let display_size = ui.io().display_size;
        let default_pos = [display_size[0] - 210.0, 10.0];
        let pos = ind.position.unwrap_or(default_pos);
        let pos_cond = if ind.locked {
            Condition::Always
        } else {
            Condition::FirstUseEver
        };

        // The window must accept inputs so the user can right-click for the
        // context menu and (when unlocked) drag it. The user opts in by
        // toggling the indicator on at all.
        let mut flags = WindowFlags::NO_DECORATION
            | WindowFlags::ALWAYS_AUTO_RESIZE
            | WindowFlags::NO_FOCUS_ON_APPEARING
            | WindowFlags::NO_NAV
            | WindowFlags::NO_SAVED_SETTINGS;
        if ind.locked {
            flags |= WindowFlags::NO_MOVE;
        }

        let max_visible = ind.max_visible.max(1) as usize;
        // Pre-compute label strings + the widest one so the mute buttons all
        // line up at the same column regardless of name length.
        let labels: Vec<String> = speaking_peers
            .iter()
            .take(max_visible)
            .map(|peer| {
                if ind.show_account_names {
                    match peer.account_name.as_deref() {
                        Some(account) if !account.is_empty() => {
                            format!("[*] {} ({})", peer.player_name, account)
                        }
                        _ => format!("[*] {}", peer.player_name),
                    }
                } else {
                    format!("[*] {}", peer.player_name)
                }
            })
            .collect();
        let mute_col_x = labels
            .iter()
            .map(|l| ui.calc_text_size(l)[0])
            .fold(0.0_f32, f32::max)
            + 12.0;

        let mut clicked: Vec<(String, bool)> = Vec::new();
        let mut live_pos: Option<[f32; 2]> = None;
        let mut new_settings: Option<SpeakingIndicatorSettings> = None;

        Window::new("##vloxximity_speaking")
            .position(pos, pos_cond)
            .flags(flags)
            .bg_alpha(ind.bg_alpha)
            .build(ui, || {
                if speaking_peers.is_empty() {
                    ui.text_disabled("(no one speaking)");
                    ui.text_disabled("right-click for options");
                }

                for (peer, label) in speaking_peers.iter().zip(labels.iter()) {
                    ui.text(label);

                    if ind.show_mute_buttons {
                        ui.same_line_with_pos(mute_col_x);
                        let btn_label = if peer.is_muted {
                            format!("Unmute##indicator_{}", peer.peer_id)
                        } else {
                            format!("Mute##indicator_{}", peer.peer_id)
                        };
                        if ui.small_button(btn_label) {
                            clicked.push((peer.peer_id.clone(), !peer.is_muted));
                        }
                    }

                    if ind.show_coordinates {
                        ui.text_disabled(format!(
                            "    ({:.0}, {:.0}, {:.0})",
                            peer.position.x, peer.position.y, peer.position.z,
                        ));
                    }
                }

                if speaking_peers.len() > max_visible {
                    ui.text(format!(
                        "... and {} more",
                        speaking_peers.len() - max_visible
                    ));
                }

                if !ind.locked {
                    live_pos = Some(ui.window_pos());
                }

                // Right-click anywhere inside the indicator opens the
                // context menu. ALLOW_WHEN_BLOCKED_BY_ACTIVE_ITEM lets it
                // fire even when a button is the active item.
                if ui.is_window_hovered_with_flags(
                    WindowHoveredFlags::ALLOW_WHEN_BLOCKED_BY_ACTIVE_ITEM,
                ) && ui.is_mouse_clicked(MouseButton::Right)
                {
                    ui.open_popup(CONTEXT_POPUP_ID);
                }

                new_settings = self.draw_context_menu(ui, &ind);
            });

        self.pending_mutes.extend(clicked);
        if let Some(ns) = new_settings {
            self.pending_settings = Some(ns);
        }

        // Persist user-driven repositioning, debounced so we don't write
        // settings.json on every frame of a drag.
        if let Some(p) = live_pos {
            if self.last_observed_pos == Some(p) {
                self.stable_frames = self.stable_frames.saturating_add(1);
            } else {
                self.last_observed_pos = Some(p);
                self.stable_frames = 0;
            }

            if self.stable_frames == POSITION_STABLE_FRAMES && ind.position != Some(p) {
                self.pending_save_position = Some(p);
            }
        } else {
            self.stable_frames = 0;
            self.last_observed_pos = None;
        }
    }

    /// Draw the right-click context popup. Returns the updated settings if
    /// the user touched any control this frame.
    fn draw_context_menu(
        &self,
        ui: &Ui,
        current: &SpeakingIndicatorSettings,
    ) -> Option<SpeakingIndicatorSettings> {
        let mut next = current.clone();
        let mut changed = false;

        ui.popup(CONTEXT_POPUP_ID, || {
            ui.text_disabled("Speaking Indicator");
            ui.separator();

            if ui.checkbox("Lock position", &mut next.locked) {
                changed = true;
            }
            if ui.checkbox("Show mute buttons", &mut next.show_mute_buttons) {
                changed = true;
            }
            if ui.checkbox("Show coordinates", &mut next.show_coordinates) {
                changed = true;
            }
            if ui.checkbox("Show account names", &mut next.show_account_names) {
                changed = true;
            }

            let mut max_visible = next.max_visible as i32;
            if Slider::new("Max visible", 1, 20).build(ui, &mut max_visible) {
                next.max_visible = max_visible.max(1) as u32;
                changed = true;
            }

            let mut alpha = next.bg_alpha;
            if Slider::new("Background alpha", 0.0, 1.0)
                .display_format("%.2f")
                .build(ui, &mut alpha)
            {
                next.bg_alpha = alpha;
                changed = true;
            }

            if next.position.is_some() {
                ui.separator();
                if ui.button("Reset position") {
                    next.position = None;
                    changed = true;
                }
            }
        });

        if changed {
            Some(next)
        } else {
            None
        }
    }

    pub fn apply_pending(&mut self, voice_manager: &mut VoiceManager) {
        let mut settings_dirty = false;
        if let Some(pos) = self.pending_save_position.take() {
            voice_manager.update_settings(|s| {
                s.speaking_indicator.position = Some(pos);
            });
            settings_dirty = true;
        }

        if let Some(ind) = self.pending_settings.take() {
            voice_manager.update_settings(|s| {
                s.speaking_indicator = ind;
            });
            settings_dirty = true;
        }

        for (peer_id, muted) in self.pending_mutes.drain(..) {
            voice_manager.mute_peer(&peer_id, muted);
        }

        if settings_dirty {
            crate::voice::persist::save_settings(&voice_manager.settings());
        }
    }
}

impl Default for SpeakingIndicator {
    fn default() -> Self {
        Self::new()
    }
}
