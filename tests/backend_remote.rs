//! Loopback tests for the remote-control backend. No sockets: the two sides
//! are driven explicitly with the same handshake and sealed-channel helpers a
//! real WebSocket server would use.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spritx_terminal::backend::remote::{
    b64, b64url, client_session, new_device_id, open_pair_request, pair_request, random,
    server_session, to_array, unb64, unb64url, ClientFrame, Control, DeviceStore, Established,
    Half, Pairing, ServerFrame, ServerHandshake, ServerStep, TabInfo, CLIENT_TO_SERVER, KEY_LEN,
    NONCE_LEN, READY, SERVER_TO_CLIENT,
};

fn temp_store(tag: &str) -> DeviceStore {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "spritx_terminal_remote_{tag}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    DeviceStore::new(root)
}

/// One side of a completed challenge; READY may or may not have been accepted
/// yet, so the wrong-key test can try first.
struct Loopback {
    server: ServerHandshake,
    store: DeviceStore,
    device_id: String,
    device_key: [u8; KEY_LEN],
    nc: [u8; NONCE_LEN],
    ns: [u8; NONCE_LEN],
    session: Established,
}

impl Loopback {
    /// Pair a device and authenticate it up to the `challenge` frame.
    fn challenge(tag: &str) -> Self {
        let store = temp_store(tag);
        let mut server = ServerHandshake::new(store.clone(), "srv-test-id", "test-host");
        let pairing = server.open_pairing();

        let device_id = new_device_id();
        let device_key = random::<KEY_LEN>();
        let request = pair_request(pairing.secret(), &device_id, &device_key, "Phone\u{7}");
        match server.handle(&request) {
            ServerStep::Reply(reply) => {
                assert_eq!(Control::from_json(&reply), Some(Control::Paired));
            }
            other => panic!("pairing failed: {other:?}"),
        }

        let nc = random::<NONCE_LEN>();
        let hello = Control::hello_request(&device_id, &nc).to_json();
        let (reply, session) = match server.handle(&hello) {
            ServerStep::Established { reply, session } => (reply, session),
            other => panic!("hello failed: {other:?}"),
        };
        let ns = match Control::from_json(&reply).expect("challenge json") {
            Control::Challenge { ns } => {
                to_array::<NONCE_LEN>(&unb64(&ns).expect("challenge nonce")).expect("24 bytes")
            }
            other => panic!("expected challenge, got {other:?}"),
        };

        Self {
            server,
            store,
            device_id,
            device_key,
            nc,
            ns,
            session,
        }
    }

    fn client_halves(&self) -> (Half, Half) {
        client_session(&self.device_key, &self.nc, &self.ns)
    }

    /// Complete the handshake: send the sealed READY proof.
    fn ready(&mut self) -> (Half, Half) {
        let (mut tx, rx) = self.client_halves();
        assert!(
            self.session.accept_ready(&tx.seal(&READY)),
            "server must accept the READY proof"
        );
        (tx, rx)
    }
}

#[test]
fn secretbox_matches_tweetnacl() {
    // Reproduced from the reference implementation: tweetnacl 1.0.3's
    // nacl.secretbox("hello spritx", nonce [100..124), key [0..32)).
    let key: [u8; KEY_LEN] = core::array::from_fn(|i| i as u8);
    let nonce: [u8; NONCE_LEN] = core::array::from_fn(|i| 100 + i as u8);
    let boxed = spritx_terminal::backend::remote::seal(&key, &nonce, b"hello spritx");
    let hex: String = boxed.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "4d8b0ad9da8338c18a797de095ede3de6adcf5a55596bd99c29457ec");
    assert_eq!(
        spritx_terminal::backend::remote::open(&key, &nonce, &boxed).unwrap(),
        b"hello spritx"
    );
}

#[test]
fn full_loopback_handshake_streams_both_directions() {
    let mut side = Loopback::challenge("loopback");
    assert_eq!(
        side.server.hello(),
        Control::Server {
            v: 2,
            id: "srv-test-id".into(),
            name: "test-host".into(),
        }
    );
    let (mut client_tx, mut client_rx) = side.ready();

    // Server → client: control frame, then terminal output.
    let tabs = ServerFrame::Control(Control::tabs(
        vec![TabInfo {
            id: 7,
            title: "~".into(),
            tab: "shell".into(),
            tab_id: 3,
        }],
        Some(7),
    ));
    let sealed = tabs.seal(&mut side.session.tx);
    assert_eq!(ServerFrame::open(&mut client_rx, &sealed), Some(tabs));

    let output = ServerFrame::Output(b"hello from the server\r\n".to_vec());
    let sealed = output.seal(&mut side.session.tx);
    assert_eq!(ServerFrame::open(&mut client_rx, &sealed), Some(output));

    // Client → server: raw input and a resize in the same channel.
    let input = ClientFrame::Input(b"echo hi\n".to_vec());
    let sealed = input.seal(&mut client_tx);
    assert_eq!(ClientFrame::open(&mut side.session.rx, &sealed), Some(input));

    let resize = ClientFrame::Resize {
        cols: 120,
        rows: 40,
    };
    let sealed = resize.seal(&mut client_tx);
    assert_eq!(
        ClientFrame::open(&mut side.session.rx, &sealed),
        Some(resize)
    );
}

#[test]
fn wrong_key_is_rejected() {
    let mut side = Loopback::challenge("wrong-key");
    assert!(side.store.get(&side.device_id).is_some());

    let (mut attacker_tx, _attacker_rx) = client_session(&random::<KEY_LEN>(), &side.nc, &side.ns);
    assert!(
        !side.session.accept_ready(&attacker_tx.seal(&READY)),
        "READY sealed with the wrong key must be refused"
    );

    // A frame sealed under the wrong session key cannot be opened either.
    let forged = ServerFrame::Output(b"forged".to_vec());
    let mut wrong_server_tx = server_session(&random::<KEY_LEN>(), &side.nc, &side.ns).0;
    let sealed = forged.seal(&mut wrong_server_tx);
    let (_, mut client_rx) = side.client_halves();
    assert_eq!(ServerFrame::open(&mut client_rx, &sealed), None);
}

#[test]
fn replay_and_reorder_are_rejected() {
    let mut side = Loopback::challenge("replay");
    let (mut client_tx, _client_rx) = side.ready();

    let first = ClientFrame::Input(b"one".to_vec()).seal(&mut client_tx);
    let second = ClientFrame::Input(b"two".to_vec()).seal(&mut client_tx);

    assert_eq!(
        ClientFrame::open(&mut side.session.rx, &first),
        Some(ClientFrame::Input(b"one".to_vec()))
    );
    // Replaying the previous frame at counter 1 fails.
    assert_eq!(ClientFrame::open(&mut side.session.rx, &first), None);
    // The genuine next frame still opens.
    assert_eq!(
        ClientFrame::open(&mut side.session.rx, &second),
        Some(ClientFrame::Input(b"two".to_vec()))
    );

    // Reordering on a fresh channel: frame 2 before frame 1 fails, then frame 1
    // (counter 0) still opens because failed opens do not advance the counter.
    let mut reordered = Loopback::challenge("reorder");
    let (mut tx, _rx) = reordered.ready();
    let a = ClientFrame::Input(b"a".to_vec()).seal(&mut tx);
    let b = ClientFrame::Input(b"b".to_vec()).seal(&mut tx);
    assert_eq!(ClientFrame::open(&mut reordered.session.rx, &b), None);
    assert_eq!(
        ClientFrame::open(&mut reordered.session.rx, &a),
        Some(ClientFrame::Input(b"a".to_vec()))
    );
}

#[test]
fn pairing_is_single_use_and_expires() {
    let store = temp_store("pairing");
    let mut server = ServerHandshake::new(store, "srv", "host");
    let pairing = server.open_pairing();
    assert_eq!(pairing.expires_in().as_secs() > 9 * 60, true);
    assert!(pairing.fragment().starts_with("#p="));
    assert!(pairing.url("http://127.0.0.1:4530/").contains("/#p="));

    let id = new_device_id();
    let key = random::<KEY_LEN>();
    let request = pair_request(pairing.secret(), &id, &key, "Laptop");
    assert!(matches!(server.handle(&request), ServerStep::Reply(_)));
    assert!(server.pairing().is_none(), "code is consumed on success");

    // Replaying the same pairing code (even with a valid seal) is refused.
    match server.handle(&request) {
        ServerStep::Denied { reply } => {
            assert_eq!(Control::from_json(&reply), Some(Control::denied("pairing")))
        }
        other => panic!("single-use pairing should be denied, got {other:?}"),
    }

    // An expired code is refused too.
    let expired = server.open_pairing_with_ttl(Duration::ZERO);
    assert!(expired.is_expired());
    let late = pair_request(expired.secret(), &new_device_id(), &random::<KEY_LEN>(), "Late");
    assert!(matches!(server.handle(&late), ServerStep::Denied { .. }));
}

#[test]
fn revoked_device_cannot_reconnect() {
    let mut side = Loopback::challenge("revoke");
    let _ = side.ready();
    assert!(side.store.get(&side.device_id).is_some());

    assert!(side.store.revoke(&side.device_id).unwrap());
    assert!(side.store.get(&side.device_id).is_none());

    let nc = random::<NONCE_LEN>();
    let hello = Control::hello_request(&side.device_id, &nc).to_json();
    match side.server.handle(&hello) {
        ServerStep::Denied { reply } => {
            assert_eq!(Control::from_json(&reply), Some(Control::denied("unknown")))
        }
        other => panic!("revoked device must be denied, got {other:?}"),
    }
}

#[test]
fn device_store_persists_lists_and_forgets() {
    let store = temp_store("store");
    let id_a = new_device_id();
    let id_b = new_device_id();
    let key = random::<KEY_LEN>();
    store.register(&id_a, &key, "Phone").unwrap();
    store.register(&id_b, &key, "Laptop").unwrap();

    let devices = store.devices();
    assert_eq!(devices.len(), 2);
    assert_eq!(store.get(&id_a).unwrap().key_bytes(), Some(key));

    // Reload through a second handle to prove persistence.
    let reopened = DeviceStore::new(store.root());
    assert_eq!(reopened.devices().len(), 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["devices.json", "server.json"] {
            let _ = store.server_id().unwrap(); // ensure server.json exists
            let path = store.root().join(name);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name} must be owner-only");
        }
    }

    assert!(store.revoke(&id_a).unwrap());
    assert!(!store.revoke(&id_a).unwrap());
    assert_eq!(store.devices().len(), 1);

    store.forget().unwrap();
    assert!(store.devices().is_empty());
}

#[test]
fn server_identity_is_stable() {
    let store = temp_store("identity");
    let first = store.server_id().unwrap();
    let second = store.server_id().unwrap();
    assert_eq!(first, second);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn control_and_client_frames_round_trip() {
    let controls = [
        Control::server_hello("id", "name"),
        Control::paired(),
        Control::hello_request("dev", &[7; NONCE_LEN]),
        Control::challenge(&[9; NONCE_LEN]),
        Control::denied("unknown"),
        Control::tabs(
            vec![TabInfo {
                id: 1,
                title: "term".into(),
                tab: "t".into(),
                tab_id: 2,
            }],
            None,
        ),
    ];
    for control in controls {
        assert_eq!(Control::from_json(&control.to_json()), Some(control));
    }

    // Pair JSON is shaped like the original browser client.
    let secret = random::<KEY_LEN>();
    let key = random::<KEY_LEN>();
    let request = pair_request(&secret, "abc123", &key, "Browser");
    let control = Control::from_json(&request).expect("pair control");
    match &control {
        Control::Pair { .. } => {}
        other => panic!("expected pair, got {other:?}"),
    }
    // And it can be decoded back into the transport pair payload.
    let payload = open_pair_request(&secret, &serde_json::from_str(&request).unwrap()).unwrap();
    assert_eq!(payload.id, "abc123");
    assert_eq!(payload.key, key);

    let frames = [
        ClientFrame::Input(b"ls -la\n".to_vec()),
        ClientFrame::Resize {
            cols: 80,
            rows: 24,
        },
        ClientFrame::Completion("src/ma".into()),
        ClientFrame::SwitchTab(42),
        ClientFrame::NewTab,
    ];
    for frame in frames {
        assert_eq!(ClientFrame::decode(&frame.encode()), frame);
    }

    let server_frames = [
        ServerFrame::Control(Control::paired()),
        ServerFrame::Output(b"\x1b[31mred\x1b[0m".to_vec()),
    ];
    for frame in server_frames {
        assert_eq!(ServerFrame::decode(&frame.encode()), Some(frame));
    }
}

#[test]
fn counter_nonces_are_directional_and_base64_matches() {
    let key: [u8; KEY_LEN] = random();
    let mut a = Half::new(key, CLIENT_TO_SERVER);
    let mut b = Half::new(key, SERVER_TO_CLIENT);
    // Same key, opposite directions: an open with the wrong direction fails.
    let frame = a.seal(b"x");
    assert!(b.open(&frame).is_none());
    assert_eq!(b.direction(), SERVER_TO_CLIENT);

    let bytes = random::<32>();
    assert_eq!(unb64(&b64(&bytes)).unwrap(), bytes);
    assert_eq!(unb64url(&b64url(&bytes)).unwrap(), bytes);

    // Pairing secrets survive the URL-fragment encoding.
    let pairing = Pairing::new();
    let fragment = pairing.fragment();
    let encoded = fragment.trim_start_matches("#p=");
    assert_eq!(
        &unb64url(encoded).unwrap()[..],
        &pairing.secret()[..],
        "fragment must carry the exact secret"
    );
}
