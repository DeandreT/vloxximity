use super::*;

fn join_room(room: &str, name: &str, key: Option<&str>) -> ClientMessage {
    ClientMessage::JoinRoom {
        room_id: room.to_string(),
        player_name: name.to_string(),
        api_key: key.map(str::to_string),
    }
}

#[test]
fn length_caps_pass_normal_messages() {
    assert!(validate_message_lengths(
        "p",
        &join_room("room", "Alice", Some("k"))
    ));
    assert!(validate_message_lengths(
        "p",
        &join_room("room", "Alice", None)
    ));
    assert!(validate_message_lengths(
        "p",
        &ClientMessage::ValidateApiKey {
            api_key: "k".to_string()
        }
    ));
    assert!(validate_message_lengths(
        "p",
        &ClientMessage::LeaveRoom { room_id: None }
    ));
    assert!(validate_message_lengths("p", &ClientMessage::Ping));
}

#[test]
fn length_caps_reject_oversize_room_id() {
    let too_long = "x".repeat(MAX_ROOM_ID_LEN + 1);
    assert!(!validate_message_lengths(
        "p",
        &join_room(&too_long, "A", None)
    ));
}

#[test]
fn length_caps_reject_oversize_player_name() {
    let too_long = "x".repeat(MAX_PLAYER_NAME_LEN + 1);
    assert!(!validate_message_lengths(
        "p",
        &join_room("r", &too_long, None)
    ));
}

#[test]
fn length_caps_reject_oversize_api_key_in_join() {
    let too_long = "x".repeat(MAX_API_KEY_LEN + 1);
    assert!(!validate_message_lengths(
        "p",
        &join_room("r", "A", Some(&too_long))
    ));
}

#[test]
fn length_caps_reject_oversize_api_key_in_validate() {
    let too_long = "x".repeat(MAX_API_KEY_LEN + 1);
    assert!(!validate_message_lengths(
        "p",
        &ClientMessage::ValidateApiKey { api_key: too_long }
    ));
}

#[test]
fn length_caps_accept_at_boundary() {
    let exact = "x".repeat(MAX_API_KEY_LEN);
    assert!(validate_message_lengths(
        "p",
        &ClientMessage::ValidateApiKey { api_key: exact }
    ));
}

#[test]
fn rate_limit_charges_correct_bucket() {
    let mut rates = PeerRateLimits::new();
    // JoinRoom bucket has burst=8; the 9th call within the same
    // instant must be rejected.
    for _ in 0..8 {
        assert!(apply_rate_limit(
            "p",
            &join_room("r", "A", None),
            &mut rates
        ));
    }
    assert!(!apply_rate_limit(
        "p",
        &join_room("r", "A", None),
        &mut rates
    ));
    // LeaveRoom and Ping bypass the buckets — they should still pass.
    assert!(apply_rate_limit(
        "p",
        &ClientMessage::LeaveRoom { room_id: None },
        &mut rates
    ));
    assert!(apply_rate_limit("p", &ClientMessage::Ping, &mut rates));
}

#[test]
fn parse_wvw_room_rest_valid() {
    assert_eq!(parse_wvw_room_rest("1001-9"), Some((1001, 9)));
    assert_eq!(parse_wvw_room_rest("2001-5"), Some((2001, 5)));
    assert_eq!(parse_wvw_room_rest("1234-23"), Some((1234, 23)));
}

#[test]
fn parse_wvw_room_rest_invalid() {
    assert_eq!(parse_wvw_room_rest(""), None);
    assert_eq!(parse_wvw_room_rest("nohyphen"), None);
    assert_eq!(parse_wvw_room_rest("abc-9"), None);
    assert_eq!(parse_wvw_room_rest("1001-xyz"), None);
    assert_eq!(parse_wvw_room_rest("-9"), None);
    assert_eq!(parse_wvw_room_rest("1001-"), None);
}

#[test]
fn parse_pvp_room_rest_valid() {
    assert_eq!(
        parse_pvp_room_rest("aabbccdd11223344-9"),
        Some(("aabbccdd11223344".to_string(), 9))
    );
    assert_eq!(
        parse_pvp_room_rest("aabbccdd11223344-5"),
        Some(("aabbccdd11223344".to_string(), 5))
    );
}

#[test]
fn parse_pvp_room_rest_invalid() {
    assert_eq!(parse_pvp_room_rest(""), None);
    assert_eq!(parse_pvp_room_rest("nohyphen"), None);
    assert_eq!(parse_pvp_room_rest("-9"), None);
    assert_eq!(parse_pvp_room_rest("aabbccdd-xyz"), None);
    assert_eq!(parse_pvp_room_rest("aabbccdd-"), None);
}

#[test]
fn pvp_team_color_accepts_known_colors() {
    assert!(is_pvp_team_color(9));
    assert!(is_pvp_team_color(5));
}

#[test]
fn pvp_team_color_rejects_unknown_colors() {
    assert!(!is_pvp_team_color(0));
    assert!(!is_pvp_team_color(23));
    assert!(!is_pvp_team_color(1));
}
