//! ImGui settings window for Vloxximity.

use crate::voice::{VoiceManager, VoiceMode, VoiceSettings};
use crate::voice::manager::ApiKeyStatus;
use crate::voice::room_type::RoomType;
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
    // Account tab state
    api_key_buffer: String,
    api_key_visible: bool,
    // Manual room-join input
    join_room_buffer: String,
    join_room_error: Option<String>,
    pending_join_room: Option<String>,
    pending_leave_rooms: Vec<String>,
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
            api_key_buffer: String::new(),
            api_key_visible: false,
            join_room_buffer: String::new(),
            join_room_error: None,
            pending_join_room: None,
            pending_leave_rooms: Vec::new(),
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

                if let Some(reason) = voice_manager.last_join_rejection() {
                    ui.text_colored(
                        [1.0, 0.4, 0.4, 1.0],
                        format!("[!] Cannot join room: {}", reason),
                    );
                    ui.text_disabled("Add or fix your API key under Account below.");
                }

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

                    ui.spacing();
                    ui.text_disabled("Per-room-type volumes (multiplied with output)");
                    let types = [
                        RoomType::Map,
                        RoomType::Squad,
                        RoomType::Party,
                    ];
                    for ty in types {
                        let mut v = new_settings.room_type_volumes.get(ty);
                        let label = format!("{}##room_vol", ty.label());
                        if Slider::new(label, 0.0_f32, 2.0_f32)
                            .display_format("%.2f")
                            .build(ui, &mut v)
                        {
                            new_settings.room_type_volumes.set(ty, v);
                            settings_changed = true;
                        }
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

                    let mut show_markers = new_settings.show_peer_markers;
                    if ui.checkbox("Show Peer Markers", &mut show_markers) {
                        new_settings.show_peer_markers = show_markers;
                        settings_changed = true;
                    }
                    ui.text_disabled("Draw world-space icons at peer positions");
                }

                ui.separator();

                // Speaking indicator overlay — only the always-relevant
                // toggles live here. The rest (lock, mute buttons,
                // coordinates, account names, max visible, alpha, reset
                // position) are accessible via right-click on the overlay
                // itself.
                if ui.collapsing_header("Speaking Indicator", TreeNodeFlags::empty()) {
                    let ind = &mut new_settings.speaking_indicator;

                    if ui.checkbox("Enable", &mut ind.enabled) {
                        settings_changed = true;
                    }
                    ui.text_disabled("Floating list of who is currently speaking");

                    if ui.checkbox("Show when nobody is speaking", &mut ind.show_when_silent) {
                        settings_changed = true;
                    }
                    ui.text_disabled("Keeps the overlay visible so you can drag it or right-click for options");
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

                // Account
                if ui.collapsing_header("Account", TreeNodeFlags::empty()) {
                    // Initialize buffer from persisted settings on first view.
                    if self.api_key_buffer.is_empty() && !new_settings.gw2_api_key.is_empty() {
                        self.api_key_buffer = new_settings.gw2_api_key.clone();
                    }

                    match voice_manager.own_account_name() {
                        Some(name) => ui.text(format!("Detected account (RTAPI): {}", name)),
                        None => ui.text_disabled("Detected account: (RTAPI not active)"),
                    }

                    ui.spacing();
                    ui.text("GW2 API Key (optional — required for persistent mutes)");

                    ui.set_next_item_width(-160.0);
                    let flags = if self.api_key_visible {
                        InputTextFlags::empty()
                    } else {
                        InputTextFlags::PASSWORD
                    };
                    ui.input_text("##gw2_api_key", &mut self.api_key_buffer)
                        .hint("Paste your GW2 API key (account scope)")
                        .flags(flags)
                        .build();
                    ui.same_line();
                    if ui.button("Paste##api_key") {
                        if let Some(text) = ui.clipboard_text() {
                            self.api_key_buffer = text.trim().to_string();
                        }
                    }
                    ui.same_line();
                    if ui.checkbox("Show", &mut self.api_key_visible) { /* toggle only */ }

                    if self.api_key_buffer.trim() != new_settings.gw2_api_key.trim() {
                        if ui.button("Save Key") {
                            new_settings.gw2_api_key = self.api_key_buffer.trim().to_string();
                            settings_changed = true;
                        }
                        ui.same_line();
                        if ui.button("Discard") {
                            self.api_key_buffer = new_settings.gw2_api_key.clone();
                        }
                        ui.text_disabled("Unsaved changes. Save, then rejoin a room to validate.");
                    } else if new_settings.gw2_api_key.is_empty() {
                        ui.text_disabled("No key set — mutes will be session-only.");
                    } else {
                        // Server-reported validation status for the saved
                        // key. `matches_current` guards against showing a
                        // stale result right after the user edits.
                        let status = voice_manager.api_key_status();
                        let matches = voice_manager.api_key_status_matches_current();
                        let color_green = [0.2, 1.0, 0.2, 1.0];
                        let color_red = [1.0, 0.4, 0.4, 1.0];
                        let color_yellow = [1.0, 0.85, 0.2, 1.0];
                        match (matches, status) {
                            (true, ApiKeyStatus::Valid { account_name }) => {
                                ui.text_colored(
                                    color_green,
                                    format!("[OK] Validated — {}", account_name),
                                );
                            }
                            (true, ApiKeyStatus::Invalid { message }) => {
                                ui.text_colored(
                                    color_red,
                                    format!("[!] Rejected — {}", message),
                                );
                            }
                            (true, ApiKeyStatus::Validating) => {
                                ui.text_colored(
                                    color_yellow,
                                    "[...] Validating with server...",
                                );
                            }
                            _ => {
                                ui.text_disabled(
                                    "Key saved. Rejoin a room to validate with the server.",
                                );
                            }
                        }
                    }

                    ui.text_disabled("Server validates the key and broadcasts your account handle");
                    ui.text_disabled("to peers. Only `account` permission is needed.");
                }

                ui.separator();

                // Server settings
                if ui.collapsing_header("Server", TreeNodeFlags::empty()) {
                    // Initialize buffer from current setting if empty
                    if self.server_url_buffer.is_empty() {
                        self.server_url_buffer = voice_manager.server_url().to_string();
                    }

                    ui.text("Server URL:");
                    ui.set_next_item_width(-80.0);
                    ui.input_text("##server_url", &mut self.server_url_buffer)
                        .hint("ws://localhost:3000/ws")
                        .flags(InputTextFlags::empty())
                        .build();
                    ui.same_line();
                    if ui.button("Paste##server_url") {
                        if let Some(text) = ui.clipboard_text() {
                            self.server_url_buffer = text.trim().to_string();
                        }
                    }

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

                // Auto-join toggle. Off by user choice → group rooms behave
                // like manual rooms (only the suggestion's Join button adds
                // them).
                let mut auto_join_group = new_settings.auto_join_group_rooms;
                if ui.checkbox("Auto-join squad/party rooms", &mut auto_join_group) {
                    new_settings.auto_join_group_rooms = auto_join_group;
                    settings_changed = true;
                }

                // RTAPI-derived suggestions (server-clustered squad/party ids).
                // Only shown when the local player is in a multi-member group
                // and the server has issued a cluster id. Click-to-join hooks
                // straight into the existing manual-join flow.
                if let Some(suggestions) = voice_manager.group_suggestions() {
                    if ui.collapsing_header("Suggested rooms", TreeNodeFlags::DEFAULT_OPEN) {
                        let already_joined: std::collections::HashSet<String> = voice_manager
                            .rooms_with_peer_counts()
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect();
                        let room_label = match suggestions.kind {
                            crate::voice::GroupKind::Squad => "Squad",
                            crate::voice::GroupKind::Party => "Party",
                            crate::voice::GroupKind::None => "Group",
                        };
                        let label_lead = match suggestions.commander_account_name.as_deref() {
                            Some(name) => format!("{}: {} ({} members)", room_label, name, suggestions.member_count),
                            None => format!("{}: {} members", room_label, suggestions.member_count),
                        };
                        ui.text(format!("{} → {}", label_lead, suggestions.room_id));
                        ui.same_line();
                        if already_joined.contains(&suggestions.room_id) {
                            ui.text_disabled("joined");
                        } else if ui.small_button(format!("Join##suggest_group")) {
                            self.pending_join_room = Some(suggestions.room_id.clone());
                            self.join_room_error = None;
                        }
                    }
                    ui.separator();
                }

                // Manual room join + active rooms list
                if ui.collapsing_header("Rooms", TreeNodeFlags::DEFAULT_OPEN) {
                    ui.text("Active rooms:");
                    let rooms = voice_manager.rooms_with_peer_counts();
                    if rooms.is_empty() {
                        ui.text_disabled("  (none)");
                    } else {
                        for (room_id, peers_count) in rooms {
                            let badge = match RoomType::from_room_id(&room_id) {
                                Some(t) => t.label(),
                                None => "Other",
                            };
                            ui.text(format!(
                                "  [{}] {}  ({} peers)",
                                badge, room_id, peers_count
                            ));
                            ui.same_line();
                            if ui.small_button(format!("Leave##{}", room_id)) {
                                self.pending_leave_rooms.push(room_id.clone());
                            }
                        }
                    }

                    ui.spacing();
                    ui.text("Join room (e.g. squad:my-squad-id)");
                    ui.set_next_item_width(-100.0);
                    ui.input_text("##join_room", &mut self.join_room_buffer)
                        .hint("squad:foo / party:bar / map:baz")
                        .flags(InputTextFlags::empty())
                        .build();
                    ui.same_line();
                    if ui.button("Join##room") {
                        let id = self.join_room_buffer.trim().to_string();
                        if id.is_empty() {
                            self.join_room_error = Some("Enter a room id first".to_string());
                        } else if RoomType::from_room_id(&id).is_none() {
                            self.join_room_error = Some(
                                "Room id must start with map:, squad:, or party:"
                                    .to_string(),
                            );
                        } else {
                            self.pending_join_room = Some(id);
                            self.join_room_buffer.clear();
                            self.join_room_error = None;
                        }
                    }
                    if let Some(err) = &self.join_room_error {
                        ui.text_colored([1.0, 0.4, 0.4, 1.0], format!("[!] {}", err));
                    }
                }

                ui.separator();

                // Peer list
                if ui.collapsing_header("Nearby Players", TreeNodeFlags::DEFAULT_OPEN) {
                    let peers = voice_manager.get_peers();

                    if peers.is_empty() {
                        ui.text_disabled("No players nearby");
                    } else {
                        // Align mute buttons across rows by computing the
                        // widest row label up front, so they don't shift
                        // around with the player names.
                        let row_labels: Vec<String> = peers
                            .iter()
                            .map(|p| {
                                let icon = if p.is_speaking { "[*]" } else { "[ ]" };
                                format!("{} {}", icon, p.player_name)
                            })
                            .collect();
                        let mute_col_x = row_labels
                            .iter()
                            .map(|l| ui.calc_text_size(l)[0])
                            .fold(0.0_f32, f32::max)
                            + 12.0;

                        for (peer, label) in peers.iter().zip(row_labels.iter()) {
                            ui.text(label);

                            // Per-peer controls
                            ui.same_line_with_pos(mute_col_x);
                            if peer.is_muted {
                                if ui.small_button(format!("Unmute##{}", peer.peer_id)) {
                                    self.pending_mutes.push((peer.peer_id.clone(), false));
                                }
                            } else {
                                if ui.small_button(format!("Mute##{}", peer.peer_id)) {
                                    self.pending_mutes.push((peer.peer_id.clone(), true));
                                }
                            }

                            match peer.account_name.as_deref() {
                                Some(name) => ui.text_disabled(format!("    account: {}", name)),
                                None => ui.text_disabled("    account: — (session-only mute)"),
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
            let old_api_key = voice_manager.settings().gw2_api_key.clone();
            let new_api_key = settings.gw2_api_key.clone();
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
                s.show_peer_markers = settings.show_peer_markers;
                s.gw2_api_key = settings.gw2_api_key;
                s.room_type_volumes = settings.room_type_volumes;
                s.auto_join_group_rooms = settings.auto_join_group_rooms;
                s.speaking_indicator = settings.speaking_indicator;
            });
            crate::voice::persist::save_settings(&voice_manager.settings());

            // If the API key actually changed, ask the server to
            // re-validate right away instead of waiting for the next room
            // rejoin.
            if old_api_key.trim() != new_api_key.trim() {
                voice_manager.revalidate_saved_api_key();
            }
        }

        // Apply pending mutes
        for (peer_id, muted) in self.pending_mutes.drain(..) {
            voice_manager.mute_peer(&peer_id, muted);
        }

        // Apply pending room operations
        if let Some(id) = self.pending_join_room.take() {
            if let Err(e) = voice_manager.join_room_manual(&id) {
                self.join_room_error = Some(e.to_string());
            }
        }
        for room_id in self.pending_leave_rooms.drain(..) {
            voice_manager.leave_room_manual(&room_id);
        }
    }
}

impl Default for SettingsWindow {
    fn default() -> Self {
        Self::new()
    }
}
