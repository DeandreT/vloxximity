use super::*;
use tokio::sync::broadcast;

fn fresh_peer(rooms: &RoomManager) -> RegisteredPeer {
    let (tx, _rx) = broadcast::channel(8);
    rooms.register_peer(tx)
}

#[test]
fn account_cap_rejects_over_limit() {
    let rooms = RoomManager::new();
    let mut bound = Vec::new();
    for _ in 0..MAX_CONNECTIONS_PER_ACCOUNT {
        let p = fresh_peer(&rooms);
        rooms
            .try_set_account_name(&p.peer_id, Some("Acct.1234".to_string()))
            .expect("under-cap binds succeed");
        bound.push(p);
    }
    assert_eq!(
        rooms.account_connection_count("Acct.1234"),
        MAX_CONNECTIONS_PER_ACCOUNT
    );

    let extra = fresh_peer(&rooms);
    let err = rooms
        .try_set_account_name(&extra.peer_id, Some("Acct.1234".to_string()))
        .unwrap_err();
    let _ = err; // AccountCapExceeded — type is the assertion.
    assert_eq!(
        rooms.account_connection_count("Acct.1234"),
        MAX_CONNECTIONS_PER_ACCOUNT,
        "rejected bind must not occupy a slot"
    );
}

#[test]
fn unregister_releases_account_slot() {
    let rooms = RoomManager::new();
    let first = fresh_peer(&rooms);
    rooms
        .try_set_account_name(&first.peer_id, Some("Acct.1234".to_string()))
        .unwrap();
    rooms.unregister_peer(&first.peer_id);
    assert_eq!(rooms.account_connection_count("Acct.1234"), 0);

    // A new peer can now bind to the same account.
    let second = fresh_peer(&rooms);
    rooms
        .try_set_account_name(&second.peer_id, Some("Acct.1234".to_string()))
        .expect("slot freed by unregister");
}

#[test]
fn rebinding_same_account_is_noop() {
    let rooms = RoomManager::new();
    let p = fresh_peer(&rooms);
    rooms
        .try_set_account_name(&p.peer_id, Some("Acct.1234".to_string()))
        .unwrap();
    rooms
        .try_set_account_name(&p.peer_id, Some("Acct.1234".to_string()))
        .expect("re-bind to same account is a no-op");
    assert_eq!(rooms.account_connection_count("Acct.1234"), 1);
}

#[test]
fn switching_accounts_moves_slot() {
    let rooms = RoomManager::new();
    let p = fresh_peer(&rooms);
    rooms
        .try_set_account_name(&p.peer_id, Some("Old.1111".to_string()))
        .unwrap();
    rooms
        .try_set_account_name(&p.peer_id, Some("New.2222".to_string()))
        .unwrap();
    assert_eq!(rooms.account_connection_count("Old.1111"), 0);
    assert_eq!(rooms.account_connection_count("New.2222"), 1);
}

#[test]
fn clearing_account_releases_slot() {
    let rooms = RoomManager::new();
    let p = fresh_peer(&rooms);
    rooms
        .try_set_account_name(&p.peer_id, Some("Acct.1234".to_string()))
        .unwrap();
    rooms.try_set_account_name(&p.peer_id, None).unwrap();
    assert_eq!(rooms.account_connection_count("Acct.1234"), 0);
}

fn drain_events(rx: &mut broadcast::Receiver<RoomEvent>) -> Vec<RoomEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[test]
fn peer_can_join_multiple_rooms() {
    let rooms = RoomManager::new();
    let (tx, _rx) = broadcast::channel(8);
    let p = rooms.register_peer(tx);

    rooms.join_room(&p.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&p.peer_id, "squad:B", "Alice").unwrap();

    let in_rooms = rooms.get_peer_rooms(&p.peer_id);
    assert!(in_rooms.contains("map:A"));
    assert!(in_rooms.contains("squad:B"));
    assert_eq!(in_rooms.len(), 2);
}

#[test]
fn rejoining_same_room_is_idempotent() {
    let rooms = RoomManager::new();
    let (tx, mut rx) = broadcast::channel(16);
    let alice = rooms.register_peer(tx);
    let (tx2, mut rx2) = broadcast::channel(16);
    let bob = rooms.register_peer(tx2);

    rooms.join_room(&alice.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&bob.peer_id, "map:A", "Bob").unwrap();
    // drain
    let _ = drain_events(&mut rx);
    let _ = drain_events(&mut rx2);

    // Alice re-joins.
    rooms.join_room(&alice.peer_id, "map:A", "Alice").unwrap();

    // Bob must NOT see another PeerJoined event for Alice.
    let evs = drain_events(&mut rx2);
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            RoomEvent::PeerJoined { peer_id, .. } if peer_id == &alice.peer_id
        )),
        "rejoin should not re-broadcast PeerJoined: got {:?}",
        evs
    );
}

#[test]
fn audio_only_fans_out_to_target_room() {
    let rooms = RoomManager::new();
    let (atx, _arx) = broadcast::channel(16);
    let alice = rooms.register_peer(atx);
    let (btx, mut brx) = broadcast::channel(16);
    let bob = rooms.register_peer(btx);
    let (ctx, mut crx) = broadcast::channel(16);
    let carol = rooms.register_peer(ctx);

    // Alice in {map:A, squad:B}; Bob in {map:A}; Carol in {squad:B}.
    rooms.join_room(&alice.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&alice.peer_id, "squad:B", "Alice").unwrap();
    rooms.join_room(&bob.peer_id, "map:A", "Bob").unwrap();
    rooms.join_room(&carol.peer_id, "squad:B", "Carol").unwrap();
    let _ = drain_events(&mut brx);
    let _ = drain_events(&mut crx);

    // Alice speaks into map:A only.
    rooms.broadcast_audio(&alice.peer_id, "map:A", b"audio".to_vec());

    let bob_evs = drain_events(&mut brx);
    let carol_evs = drain_events(&mut crx);
    assert!(bob_evs.iter().any(|e| matches!(
        e,
        RoomEvent::PeerAudio { room_id, .. } if room_id == "map:A"
    )));
    assert!(
        !carol_evs
            .iter()
            .any(|e| matches!(e, RoomEvent::PeerAudio { .. })),
        "carol is in squad:B only and must not receive map:A audio"
    );
}

#[test]
fn audio_dropped_if_sender_not_in_target_room() {
    let rooms = RoomManager::new();
    let (atx, _arx) = broadcast::channel(16);
    let alice = rooms.register_peer(atx);
    let (btx, mut brx) = broadcast::channel(16);
    let bob = rooms.register_peer(btx);

    rooms.join_room(&alice.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&bob.peer_id, "squad:B", "Bob").unwrap();
    let _ = drain_events(&mut brx);

    // Alice tries to inject into squad:B (which she didn't join).
    rooms.broadcast_audio(&alice.peer_id, "squad:B", b"audio".to_vec());
    let evs = drain_events(&mut brx);
    assert!(
        !evs.iter().any(|e| matches!(e, RoomEvent::PeerAudio { .. })),
        "audio targeted at unjoined room must be dropped"
    );
}

#[test]
fn position_dedup_across_shared_rooms() {
    let rooms = RoomManager::new();
    let (atx, _arx) = broadcast::channel(16);
    let alice = rooms.register_peer(atx);
    let (btx, mut brx) = broadcast::channel(16);
    let bob = rooms.register_peer(btx);

    // Both Alice and Bob are in two shared rooms.
    rooms.join_room(&alice.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&alice.peer_id, "squad:B", "Alice").unwrap();
    rooms.join_room(&bob.peer_id, "map:A", "Bob").unwrap();
    rooms.join_room(&bob.peer_id, "squad:B", "Bob").unwrap();
    let _ = drain_events(&mut brx);

    rooms.update_position(
        &alice.peer_id,
        Position::new(1.0, 2.0, 3.0),
        Position::new(0.0, 0.0, 1.0),
    );

    let position_count = drain_events(&mut brx)
        .into_iter()
        .filter(|e| matches!(e, RoomEvent::PeerPosition { .. }))
        .count();
    assert_eq!(
        position_count, 1,
        "Bob should receive exactly one position update"
    );
}

#[test]
fn leave_room_specific_id() {
    let rooms = RoomManager::new();
    let (tx, _rx) = broadcast::channel(8);
    let p = rooms.register_peer(tx);

    rooms.join_room(&p.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&p.peer_id, "squad:B", "Alice").unwrap();

    rooms.leave_room(&p.peer_id, "map:A");
    let in_rooms = rooms.get_peer_rooms(&p.peer_id);
    assert!(!in_rooms.contains("map:A"));
    assert!(in_rooms.contains("squad:B"));
}

#[test]
fn leave_all_rooms_clears_membership() {
    let rooms = RoomManager::new();
    let (tx, _rx) = broadcast::channel(8);
    let p = rooms.register_peer(tx);

    rooms.join_room(&p.peer_id, "map:A", "Alice").unwrap();
    rooms.join_room(&p.peer_id, "squad:B", "Alice").unwrap();
    rooms.leave_all_rooms(&p.peer_id);

    assert!(rooms.get_peer_rooms(&p.peer_id).is_empty());
    assert_eq!(rooms.room_count(), 0, "rooms should clean up when empty");
}
