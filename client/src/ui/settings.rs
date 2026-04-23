//! ImGui settings window for Vloxximity.

use crate::voice::{VoiceManager, VoiceMode, VoiceSettings};
use nexus::imgui::{Condition, InputTextFlags, Selectable, Slider, TreeNodeFlags, Ui, Window};

/// Settings window state
pub struct SettingsWindow {
    is_open: bool,
    selected_input_device: usize,
    selected_output_device: usize,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    ptt_key_listening: bool,
    // Pending actions to apply
    pending_mutes: Vec<(String, bool)>,
    pending_settings: Option<VoiceSettings>,
    // Server URL editing
    server_url_buffer: String,
    pending_server_url: Option<String>,
    // Pending device selections (device name)
    pending_input_device: Option<String>,
    pending_output_device: Option<String>,
}

impl SettingsWindow {
    pub fn new() -> Self {
        Self {
            is_open: false,
            selected_input_device: 0,
            selected_output_device: 0,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            ptt_key_listening: false,
            pending_mutes: Vec::new(),
            pending_settings: None,
            server_url_buffer: String::new(),
            pending_server_url: None,
            pending_input_device: None,
            pending_output_device: None,
        }
    }

    /// Open the settings window
    pub fn open(&mut self) {
        self.is_open = true;
        self.refresh_devices();
    }

    /// Close the settings window
    pub fn close(&mut self) {
        self.is_open = false;
    }

    /// Toggle settings window
    pub fn toggle(&mut self) {
        self.is_open = !self.is_open;
        if self.is_open {
            self.refresh_devices();
        }
    }

    /// Check if window is open
    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Refresh device lists by querying cpal on the calling thread.
    pub fn refresh_devices(&mut self) {
        let previous_input = self.input_devices.get(self.selected_input_device).cloned();
        let previous_output = self
            .output_devices
            .get(self.selected_output_device)
            .cloned();

        self.input_devices = crate::audio::list_input_devices();
        self.output_devices = crate::audio::list_output_devices();

        let default_input = crate::audio::default_input_device_name();
        let default_output = crate::audio::default_output_device_name();

        self.selected_input_device = previous_input
            .as_deref()
            .or(default_input.as_deref())
            .and_then(|name| self.input_devices.iter().position(|d| d == name))
            .unwrap_or(0);
        self.selected_output_device = previous_output
            .as_deref()
            .or(default_output.as_deref())
            .and_then(|name| self.output_devices.iter().position(|d| d == name))
            .unwrap_or(0);
    }

    /// Render the settings window (read-only access to VoiceManager)
    pub fn render(&mut self, ui: &Ui, voice_manager: &VoiceManager) {
        if !self.is_open {
            return;
        }

        let settings = voice_manager.settings();
        let mut new_settings = settings.clone();
        let mut settings_changed = false;

        Window::new("Vloxximity Voice Chat")
            .size([400.0, 500.0], Condition::FirstUseEver)
            .opened(&mut self.is_open)
            .build(ui, || {
                // Connection status
                let state = voice_manager.state();
                let status_text = match state {
                    crate::voice::VoiceState::Disconnected => "Disconnected",
                    crate::voice::VoiceState::Connecting => "Connecting...",
                    crate::voice::VoiceState::Connected => "Connected",
                    crate::voice::VoiceState::InRoom => "In Room",
                };

                ui.text(format!("Status: {}", status_text));
                ui.text(format!("Peers: {}", voice_manager.peer_count()));
                ui.separator();

                // Voice mode selection
                if ui.collapsing_header("Voice Mode", TreeNodeFlags::DEFAULT_OPEN) {
                    let mut mode_idx = match new_settings.mode {
                        VoiceMode::PushToTalk => 0,
                        VoiceMode::VoiceActivity => 1,
                        VoiceMode::AlwaysOn => 2,
                    };

                    if ui.radio_button("Push to Talk", &mut mode_idx, 0) {
                        new_settings.mode = VoiceMode::PushToTalk;
                        settings_changed = true;
                    }
                    if ui.radio_button("Voice Activity", &mut mode_idx, 1) {
                        new_settings.mode = VoiceMode::VoiceActivity;
                        settings_changed = true;
                    }
                    if ui.radio_button("Always On", &mut mode_idx, 2) {
                        new_settings.mode = VoiceMode::AlwaysOn;
                        settings_changed = true;
                    }

                    // PTT key binding
                    if new_settings.mode == VoiceMode::PushToTalk {
                        ui.spacing();
                        let key_text = if self.ptt_key_listening {
                            "Press a key..."
                        } else if new_settings.ptt_key == 0 {
                            "Not bound"
                        } else {
                            "Bound" // Would show actual key name
                        };

                        if ui.button(format!("PTT Key: {}", key_text)) {
                            self.ptt_key_listening = true;
                        }
                    }
                }

                ui.separator();

                // Audio devices
                if ui.collapsing_header("Audio Devices", TreeNodeFlags::DEFAULT_OPEN) {
                    // Input device
                    ui.text("Input Device:");
                    let input_preview = self
                        .input_devices
                        .get(self.selected_input_device)
                        .map(|s| s.as_str())
                        .unwrap_or("None");

                    if let Some(_combo) = ui.begin_combo("##input", input_preview) {
                        for (i, device) in self.input_devices.iter().enumerate() {
                            if Selectable::new(device).build(ui) {
                                if i != self.selected_input_device {
                                    self.selected_input_device = i;
                                    self.pending_input_device = Some(device.clone());
                                }
                            }
                        }
                    }

                    ui.spacing();

                    // Output device
                    ui.text("Output Device:");
                    let output_preview = self
                        .output_devices
                        .get(self.selected_output_device)
                        .map(|s| s.as_str())
                        .unwrap_or("None");

                    if let Some(_combo) = ui.begin_combo("##output", output_preview) {
                        for (i, device) in self.output_devices.iter().enumerate() {
                            if Selectable::new(device).build(ui) {
                                if i != self.selected_output_device {
                                    self.selected_output_device = i;
                                    self.pending_output_device = Some(device.clone());
                                }
                            }
                        }
                    }
                }

                ui.separator();

                // Volume controls
                if ui.collapsing_header("Volume", TreeNodeFlags::DEFAULT_OPEN) {
                    // Input volume
                    let mut input_vol = new_settings.input_volume * 100.0;
                    if Slider::new("Input Volume", 0.0, 100.0)
                        .display_format("%.2f")
                        .build(ui, &mut input_vol)
                    {
                        new_settings.input_volume = input_vol / 100.0;
                        settings_changed = true;
                    }

                    // Output volume
                    let mut output_vol = new_settings.output_volume * 100.0;
                    if Slider::new("Output Volume", 0.0, 100.0)
                        .display_format("%.2f")
                        .build(ui, &mut output_vol)
                    {
                        new_settings.output_volume = output_vol / 100.0;
                        settings_changed = true;
                    }
                }

                ui.separator();

                // Distance settings
                if ui.collapsing_header("Hearing Range", TreeNodeFlags::DEFAULT_OPEN) {
                    ui.text("Distance in GW2 units (inches)");

                    let mut min_dist = new_settings.min_distance;
                    if Slider::new("Min Distance", 0.0f32, 5000.0f32).build(ui, &mut min_dist) {
                        new_settings.min_distance = min_dist;
                        settings_changed = true;
                    }
                    ui.text_disabled("Full volume within this range");

                    let mut max_dist = new_settings.max_distance;
                    if Slider::new("Max Distance", 1000.0f32, 100000.0f32).build(ui, &mut max_dist) {
                        new_settings.max_distance = max_dist;
                        settings_changed = true;
                    }
                    ui.text_disabled("Inaudible beyond this range");
                }

                ui.separator();

                // Spatial audio
                if ui.collapsing_header("Spatial Audio", TreeNodeFlags::DEFAULT_OPEN) {
                    let mut directional = new_settings.directional_audio_enabled;
                    if ui.checkbox("Directional Audio", &mut directional) {
                        new_settings.directional_audio_enabled = directional;
                        settings_changed = true;
                    }
                    ui.text_disabled("Off: all peers play centered (mono)");

                    if new_settings.directional_audio_enabled {
                        ui.indent();
                        let mut spatial_3d = new_settings.spatial_3d_enabled;
                        if ui.checkbox("3D Spatial Audio", &mut spatial_3d) {
                            new_settings.spatial_3d_enabled = spatial_3d;
                            settings_changed = true;
                        }
                        ui.text_disabled("On: ITD + front/back filter. Off: 2D pan.");
                        ui.unindent();
                    }
                }

                ui.separator();

                // Mute/Deaf controls
                if ui.collapsing_header("Controls", TreeNodeFlags::DEFAULT_OPEN) {
                    let mut muted = new_settings.is_muted;
                    if ui.checkbox("Mute Microphone", &mut muted) {
                        new_settings.is_muted = muted;
                        settings_changed = true;
                    }

                    let mut deafened = new_settings.is_deafened;
                    if ui.checkbox("Deafen (mute all)", &mut deafened) {
                        new_settings.is_deafened = deafened;
                        settings_changed = true;
                    }
                }

                ui.separator();

                // Server settings
                if ui.collapsing_header("Server", TreeNodeFlags::empty()) {
                    // Initialize buffer from current setting if empty
                    if self.server_url_buffer.is_empty() {
                        self.server_url_buffer = voice_manager.server_url().to_string();
                    }

                    ui.text("Server URL:");
                    ui.set_next_item_width(-1.0);
                    ui.input_text("##server_url", &mut self.server_url_buffer)
                        .hint("ws://localhost:3000/ws")
                        .flags(InputTextFlags::empty())
                        .build();

                    ui.spacing();

                    let current_url = voice_manager.server_url();
                    let url_changed = self.server_url_buffer != current_url;

                    if url_changed {
                        if ui.button("Apply & Reconnect") {
                            self.pending_server_url = Some(self.server_url_buffer.clone());
                        }
                        ui.same_line();
                        if ui.button("Reset") {
                            self.server_url_buffer = current_url.to_string();
                        }
                    } else {
                        ui.text_disabled("Server URL unchanged");
                    }
                }

                ui.separator();

                // Peer list
                if ui.collapsing_header("Nearby Players", TreeNodeFlags::DEFAULT_OPEN) {
                    let peers = voice_manager.get_peers();

                    if peers.is_empty() {
                        ui.text_disabled("No players nearby");
                    } else {
                        for peer in peers {
                            let icon = if peer.is_speaking { "[*]" } else { "[ ]" };
                            ui.text(format!("{} {}", icon, peer.player_name));

                            // Per-peer controls
                            ui.same_line();
                            if peer.is_muted {
                                if ui.small_button(format!("Unmute##{}", peer.peer_id)) {
                                    self.pending_mutes.push((peer.peer_id.clone(), false));
                                }
                            } else {
                                if ui.small_button(format!("Mute##{}", peer.peer_id)) {
                                    self.pending_mutes.push((peer.peer_id.clone(), true));
                                }
                            }

                            ui.text_disabled(format!(
                                "    pos ({:.1}, {:.1}, {:.1})",
                                peer.position.x, peer.position.y, peer.position.z,
                            ));
                            match peer.distance {
                                Some(d) => ui.text_disabled(format!("    distance {:.1}", d)),
                                None => ui.text_disabled("    distance —"),
                            }
                        }
                    }
                }

            });

        // Store new settings if changed (will be applied by caller)
        if settings_changed {
            self.pending_settings = Some(new_settings);
        }
    }

    /// Apply any pending changes to the voice manager
    pub fn apply_pending(&mut self, voice_manager: &mut VoiceManager) {
        // Apply pending server URL change (must be done first as it may restart connection)
        if let Some(url) = self.pending_server_url.take() {
            voice_manager.set_server_url(&url);
        }

        if let Some(name) = self.pending_input_device.take() {
            voice_manager.set_input_device(&name);
        }
        if let Some(name) = self.pending_output_device.take() {
            voice_manager.set_output_device(&name);
        }

        // Apply pending settings
        if let Some(settings) = self.pending_settings.take() {
            voice_manager.update_settings(|s| {
                s.mode = settings.mode;
                s.input_volume = settings.input_volume;
                s.output_volume = settings.output_volume;
                s.min_distance = settings.min_distance;
                s.max_distance = settings.max_distance;
                s.is_muted = settings.is_muted;
                s.is_deafened = settings.is_deafened;
                s.directional_audio_enabled = settings.directional_audio_enabled;
                s.spatial_3d_enabled = settings.spatial_3d_enabled;
            });
        }

        // Apply pending mutes
        for (peer_id, muted) in self.pending_mutes.drain(..) {
            voice_manager.mute_peer(&peer_id, muted);
        }
    }
}

impl Default for SettingsWindow {
    fn default() -> Self {
        Self::new()
    }
}
