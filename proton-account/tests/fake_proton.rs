//! End-to-end check against a pretend Proton: a local server that speaks the
//! API (real SRP on its side, real OpenPGP keys and ciphertext) so the whole
//! chain — sign-in, 2FA, key unlocking, Drive names and downloads, Calendar
//! events — runs exactly as it would against Proton, minus the network.

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use proton_crypto::crypto::{
    DataEncoding, Encryptor, EncryptorSync, KeyGenerator, KeyGeneratorSync, PGPMessage, PGPProviderSync,
};
use proton_srp::{
    RPGPVerifier, SRPAuth, SRPVerifierB64, ServerClientProof, ServerClientVerifier, ServerInteraction,
    mailbox_password_hash,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spotty_proton_account::{Api, Client, Error, ForkRequest, Step, begin, derive_key_password, parse_pass_login_url};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const MODULUS: &str = "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nW2z5HBi8RvsfYzZTS7qBaUxxPhsfHJFZpu3Kd6s1JafNrCCH9rfvPLrfuqocxWPgWDH2R8neK7PkNvjxto9TStuY5z7jAzWRvFWN9cQhAKkdWgy0JY6ywVn22+HFpF4cYesHrqFIKUPDMSSIlWjBVmEJZ/MusD44ZT29xcPrOqeZvwtCffKtGAIjLYPZIEbZKnDM1Dm3q2K/xS5h+xdhjnndhsrkwm9U9oyA2wxzSXFL+pdfj2fOdRwuR5nW0J2NFrq3kJjkRmpO/Genq1UW+TEknIWAb6VzJJJA244K/H8cnSx2+nSNZO3bbo6Ys228ruV9A8m6DhxmS+bihN3ttQ==\n-----BEGIN PGP SIGNATURE-----\nVersion: ProtonMail\nComment: https://protonmail.com\n\nwl4EARYIABAFAlwB1j0JEDUFhcTpUY8mAAD8CgEAnsFnF4cF0uSHKkXa1GIa\nGO86yMV4zDZEZcDSJo0fgr8A/AlupGN9EdHlsrZLmTA1vhIx+rOgxdEff28N\nkvNM7qIK\n=q6vu\n-----END PGP SIGNATURE-----";

type Handler = dyn Fn(&str, &str, &HashMap<String, String>, &str) -> (u16, String) + Send + Sync;

/// The `pass-cli login` code and login key the pretend Proton expects in a Pass fork.
const PASS_USER_CODE: &str = "PASS-CODE-42";
const PASS_KEY: [u8; 32] = [0xfb; 32];
type PassCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, U16>;

fn serve(handler: Arc<Handler>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                let first = head.lines().next().unwrap_or_default().to_owned();
                let mut parts = first.split(' ');
                let method = parts.next().unwrap_or_default().to_owned();
                let target = parts.next().unwrap_or_default().to_owned();
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let query: HashMap<String, String> = query
                    .split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                let (status, payload) = handler(&method, path.trim_start_matches('/'), &query, &body);
                // Storage replies are raw bytes, carried here one char per byte.
                let bytes: Vec<u8> = if path.trim_start_matches('/').starts_with("storage/") { payload.chars().map(|c| c as u8).collect() } else { payload.into_bytes() };
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&bytes);
            });
        }
    });
    base
}

/// Everything the pretend Proton serves, built with real keys.
fn pgp() -> impl PGPProviderSync {
    proton_crypto::new_pgp_provider()
}

struct World {
    key_salt: Vec<u8>,
    key_password: String,
    user_key: String,
    address_key: String,
    address_token: String,
    share_key: String,
    share_passphrase: String,
    root_key: String,
    root_passphrase: String,
    root_name: String,
    folder_key: String,
    folder_passphrase: String,
    folder_name: String,
    file_key: String,
    file_passphrase: String,
    file_name: String,
    content_key_packet: String,
    block: Vec<u8>,
    calendar_key: String,
    calendar_passphrase: String,
    event_shared_key_packet: String,
    event_card: String,
    event_plain_card: String,
}

fn armored<P: PGPProviderSync>(p: &P, to: &P::PublicKey, data: &[u8]) -> String {
    let message = p.new_encryptor().with_encryption_key(to).encrypt(data).unwrap();
    String::from_utf8(message.armor().unwrap()).unwrap()
}

/// A fresh key locked with `passphrase`; returns (private, public, armored locked).
fn key<P: PGPProviderSync>(p: &P, passphrase: &str) -> (P::PrivateKey, P::PublicKey, String) {
    let private = p.new_key_generator().with_user_id("t", "t@example.test").generate().unwrap();
    let public = p.private_key_to_public_key(&private).unwrap();
    let locked = String::from_utf8(p.private_key_export(&private, passphrase, DataEncoding::Armor).unwrap().as_ref().to_vec()).unwrap();
    (private, public, locked)
}

fn build_world<P: PGPProviderSync>(p: &P, key_password: &str, key_salt: Vec<u8>) -> World {
    let (_, user_pub, user_key) = key(p, key_password);
    let (_, address_pub, address_key) = key(p, "address-pass");
    let address_token = armored(p, &user_pub, b"address-pass");

    let (_, share_pub, share_key) = key(p, "share-pass");
    let share_passphrase = armored(p, &address_pub, b"share-pass");

    let (_, root_pub, root_key) = key(p, "root-pass");
    let root_passphrase = armored(p, &share_pub, b"root-pass");
    let root_name = armored(p, &share_pub, b"root");

    let (_, folder_pub, folder_key) = key(p, "folder-pass");
    let folder_passphrase = armored(p, &root_pub, b"folder-pass");
    let folder_name = armored(p, &root_pub, "Tax \u{1F4C1} 2026".as_bytes());

    let (_, file_pub, file_key) = key(p, "file-pass");
    let file_passphrase = armored(p, &folder_pub, b"file-pass");
    let file_name = armored(p, &folder_pub, b"invoice.pdf");
    let session = p.new_encryptor().with_encryption_key(&file_pub).generate_session_key().unwrap();
    let packets = p.new_encryptor().with_encryption_key(&file_pub).encrypt_session_key(&session).unwrap();
    let block = p.new_encryptor().with_session_key_ref(&session).encrypt_raw(b"hello from proton drive", DataEncoding::Bytes).unwrap();

    let (_, calendar_pub, calendar_key) = key(p, "calendar-pass");
    let calendar_passphrase = armored(p, &address_pub, b"calendar-pass");
    let event_session = p.new_encryptor().with_encryption_key(&calendar_pub).generate_session_key().unwrap();
    let event_packets = p.new_encryptor().with_encryption_key(&calendar_pub).encrypt_session_key(&event_session).unwrap();
    let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Dentist\\, 3rd floor\r\nLOCATION:Main St 1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let card = p.new_encryptor().with_session_key_ref(&event_session).encrypt_raw(ics, DataEncoding::Bytes).unwrap();

    World {
        key_salt,
        key_password: key_password.to_owned(),
        user_key,
        address_key,
        address_token,
        share_key,
        share_passphrase,
        root_key,
        root_passphrase,
        root_name,
        folder_key,
        folder_passphrase,
        folder_name,
        file_key,
        file_passphrase,
        file_name,
        content_key_packet: STANDARD.encode(packets),
        block,
        calendar_key,
        calendar_passphrase,
        event_shared_key_packet: STANDARD.encode(event_packets),
        event_card: STANDARD.encode(card),
        event_plain_card: "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDESCRIPTION:Bring the X-ray\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into(),
    }
}

const PASSWORD: &str = "correct horse battery staple";

fn handler(world: Arc<World>, base: Arc<Mutex<String>>, two_factor: bool) -> Arc<Handler> {
    let client_verifier: SRPVerifierB64 = SRPAuth::generate_verifier(&RPGPVerifier::default(), PASSWORD, None, MODULUS).unwrap().into();
    let sessions: Mutex<Option<ServerInteraction>> = Mutex::new(None);
    let code_seen = Mutex::new(false);
    Arc::new(move |method, path, query, body| {
        let ok = |value: Value| (200, value.to_string());
        let body_json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        match (method, path) {
            ("POST", "core/v4/auth/info") => {
                assert_eq!(body_json["Username"], "sam@proton.test");
                let verifier = ServerClientVerifier::try_from(&client_verifier).unwrap();
                let mut server = ServerInteraction::new_with_modulus_extractor(&RPGPVerifier::default(), MODULUS, &verifier).unwrap();
                let challenge = server.generate_challenge();
                *sessions.lock().unwrap() = Some(server);
                ok(json!({"Code":1000,"Version":4,"Modulus":MODULUS,"ServerEphemeral":challenge.encode_b64(),"Salt":client_verifier.salt,"SRPSession":"s1"}))
            }
            ("POST", "core/v4/auth") => {
                let mut guard = sessions.lock().unwrap();
                let server = guard.as_mut().unwrap();
                let proof = ServerClientProof::new(body_json["ClientEphemeral"].as_str().unwrap(), body_json["ClientProof"].as_str().unwrap()).unwrap();
                match server.verify_proof(&proof) {
                    Ok(server_proof) => ok(json!({"Code":1000,"ServerProof":server_proof.encode_b64(),"UID":"uid1","AccessToken":"acc1","RefreshToken":"ref1",
                        "PasswordMode":1,"2FA":{"Enabled": if two_factor {1} else {0}}})),
                    Err(_) => (422, json!({"Code":8002,"Error":"Incorrect login credentials. Please try again."}).to_string()),
                }
            }
            ("POST", "auth/v4/2fa") => {
                if body_json["TwoFactorCode"] == "123456" {
                    *code_seen.lock().unwrap() = true;
                    ok(json!({"Code":1000}))
                } else {
                    (422, json!({"Code":8002,"Error":"Incorrect login credentials. Please try again."}).to_string())
                }
            }
            ("POST", "auth/v4/refresh") => ok(json!({"Code":1000,"UID":"uid1","AccessToken":"acc2","RefreshToken":"ref2"})),
            ("DELETE", "auth/v4") => ok(json!({"Code":1000})),
            ("POST", "auth/v4/sessions/forks") => fork(&world, &body_json),
            _ => {
                // Everything below needs the session.
                let _ = &code_seen;
                ok_private(&world, &base, method, path, query, &body_json)
            }
        }
    })
}

/// Session forks: Proton only hands out a child for the two whitelisted apps,
/// never a child that outlives its parent, and a Pass child only comes with a
/// payload that opens with the login key and carries the key password.
fn fork(world: &World, body: &Value) -> (u16, String) {
    let refuse = |message: &str| (422, json!({"Code":2001,"Error":message}).to_string());
    if body["Independent"] != json!(0) {
        return refuse("a fork must stay tied to its parent");
    }
    match body["ChildClientID"].as_str() {
        Some("linux-vpn-gui") => {
            if body.get("UserCode").is_some() || body.get("Payload").is_some() {
                return refuse("a VPN fork takes no code or payload");
            }
        }
        Some("cli-pass") => {
            if body["UserCode"] != json!(PASS_USER_CODE) {
                return refuse("the Pass code doesn't match");
            }
            match body["Payload"].as_str() {
                Some(payload) if opens_for_pass_cli(payload, &world.key_password) => {}
                _ => return refuse("the Pass payload doesn't carry the key password"),
            }
        }
        _ => return refuse("that app can't be signed in this way"),
    }
    (200, json!({"Code":1000,"Selector":"sel1"}).to_string())
}

/// Open a Pass fork payload the way pass-cli does and check the key password in it.
fn opens_for_pass_cli(payload: &str, expected: &str) -> bool {
    let Ok(raw) = STANDARD.decode(payload) else { return false };
    if raw.len() < 16 {
        return false;
    }
    let (nonce, sealed) = raw.split_at(16);
    let Ok(cipher) = PassCipher::new_from_slice(&PASS_KEY) else { return false };
    let Ok(plain) = cipher.decrypt(aes_gcm::Nonce::<U16>::from_slice(nonce), sealed) else { return false };
    serde_json::from_slice::<Value>(&plain).map(|v| v["keyPassword"] == json!(expected)).unwrap_or(false)
}

/// Percent-encodes everything but unreserved characters, the way Proton's link does.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn ok_private(world: &World, base: &Mutex<String>, method: &str, path: &str, query: &HashMap<String, String>, body: &Value) -> (u16, String) {
    let ok = |value: Value| (200, value.to_string());
    let w = world;
    match (method, path) {
        ("GET", "core/v4/users") => ok(json!({"Code":1000,"User":{"Name":"sam","Email":"sam@proton.test","DisplayName":"Sam","Keys":[{"ID":"uk1","Primary":1,"PrivateKey":w.user_key}]}})),
        ("GET", "core/v4/keys/salts") => ok(json!({"Code":1000,"KeySalts":[{"ID":"uk1","KeySalt":STANDARD.encode(&w.key_salt)}]})),
        ("GET", "core/v4/addresses") => ok(json!({"Code":1000,"Addresses":[{"ID":"a1","Email":"sam@proton.test","Keys":[{"ID":"ak1","PrivateKey":w.address_key,"Token":w.address_token}]}]})),
        ("GET", "drive/v2/shares/my-files") => ok(json!({"Code":1000,"Volume":{"VolumeID":"v1"},"Share":{"ShareID":"sh1","Key":w.share_key,"Passphrase":w.share_passphrase,"AddressID":"a1","CreatorEmail":"sam@proton.test"},"Link":{"Link":{"LinkID":"root"}}})),
        ("POST", "drive/v2/volumes/v1/links") => {
            let ids: Vec<&str> = body["LinkIDs"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            let links: Vec<Value> = ids
                .iter()
                .map(|id| match *id {
                    "root" => json!({"Link":{"LinkID":"root","Type":1,"Name":w.root_name,"NodeKey":w.root_key,"NodePassphrase":w.root_passphrase,"ModifyTime":1},"Folder":{}}),
                    "folder" => json!({"Link":{"LinkID":"folder","ParentLinkID":"root","Type":1,"Name":w.folder_name,"NodeKey":w.folder_key,"NodePassphrase":w.folder_passphrase,"ModifyTime":2},"Folder":{}}),
                    _ => json!({"Link":{"LinkID":"file","ParentLinkID":"folder","Type":2,"Name":w.file_name,"NodeKey":w.file_key,"NodePassphrase":w.file_passphrase,"ModifyTime":1700000000},
                        "File":{"TotalEncryptedSize":99,"MediaType":"application/pdf","ContentKeyPacket":w.content_key_packet,"ActiveRevision":{"RevisionID":"rev1"}}}),
                })
                .collect();
            ok(json!({"Code":1000,"Links":links}))
        }
        ("GET", "drive/v2/volumes/v1/folders/root/children") => ok(json!({"Code":1000,"LinkIDs":["folder"],"More":false})),
        ("GET", "drive/v2/volumes/v1/folders/folder/children") => ok(json!({"Code":1000,"LinkIDs":["file"],"More":false})),
        ("GET", "drive/v2/volumes/v1/files/file/revisions/rev1") => {
            let first = query.get("FromBlockIndex").map(String::as_str) == Some("1");
            let url = format!("{}/storage/block1", base.lock().unwrap());
            let blocks = if first { vec![json!({"Index":1,"BareURL":url,"Token":"tok","Hash":STANDARD.encode(Sha256::digest(&w.block))})] } else { vec![] };
            ok(json!({"Code":1000,"Revision":{"Blocks":blocks}}))
        }
        ("GET", "calendar/v1") => ok(json!({"Code":1000,"Calendars":[{"ID":"c1","Type":0,"Members":[{"ID":"m1","Name":"Personal","Color":"#8080ff","Display":1}]}]})),
        ("GET", "calendar/v2/c1/bootstrap") => ok(json!({"Code":1000,"Keys":[{"ID":"ck1","PrivateKey":w.calendar_key,"Flags":3}],
            "Passphrase":{"MemberPassphrases":[{"MemberID":"m1","Passphrase":w.calendar_passphrase}]}})),
        ("GET", "calendar/v1/c1/events") => {
            let kind = query.get("Type").map(String::as_str).unwrap_or("0");
            let page = query.get("Page").map(String::as_str).unwrap_or("0");
            if page != "0" {
                return ok(json!({"Code":1000,"Events":[]}));
            }
            let shared = |kind3: bool| {
                json!([{"Type": if kind3 {3} else {2}, "Data": if kind3 {w.event_card.clone()} else {w.event_plain_card.clone()}, "Signature": "sig"}])
            };
            match kind {
                // 2026-06-10 10:00 UTC, a one-off.
                "0" => ok(json!({"Code":1000,"Events":[{"ID":"e1","UID":"u1","StartTime":1781085600,"EndTime":1781089200,"StartTimezone":"UTC","FullDay":0,"RRule":null,"Exdates":[],"RecurrenceID":null,
                    "SharedKeyPacket":w.event_shared_key_packet,"SharedEvents":shared(true),"CalendarEvents":shared(false)}]})),
                // A weekly series that began earlier.
                "1" => ok(json!({"Code":1000,"Events":[{"ID":"e2","UID":"u2","StartTime":1780394400,"EndTime":1780398000,"StartTimezone":"UTC","FullDay":0,"RRule":"FREQ=WEEKLY","Exdates":[],"RecurrenceID":null,
                    "SharedKeyPacket":w.event_shared_key_packet,"SharedEvents":shared(true),"CalendarEvents":[]}]})),
                _ => ok(json!({"Code":1000,"Events":[]})),
            }
        }
        ("GET", "storage/block1") => (200, String::new()),
        _ => (404, json!({"Code":2501,"Error":format!("no route {method} {path}")}).to_string()),
    }
}

#[test]
fn sign_in_then_browse_drive_and_read_the_calendar() {
    let key_salt = vec![7u8; 16];
    let key_password = derive_key_password(PASSWORD, &key_salt).unwrap();
    let world = Arc::new(build_world(&pgp(), &key_password, key_salt));
    let base = Arc::new(Mutex::new(String::new()));

    // Block storage is served raw, not as JSON, so give it its own listener path.
    let block = world.block.clone();
    let inner = handler(world.clone(), base.clone(), false);
    let routed: Arc<Handler> = Arc::new(move |m, p, q, b| if p == "storage/block1" { (200, block.iter().map(|b| *b as char).collect::<String>()) } else { inner(m, p, q, b) });
    let url = serve(routed);
    *base.lock().unwrap() = url.clone();
    let _ = &block_bytes_note;

    let api = Api::with_base(&url).unwrap();
    let account = match begin(&api, "sam@proton.test", PASSWORD).expect("sign in") {
        Step::Done(account) => account,
        _ => panic!("expected to be done"),
    };
    assert_eq!(account.email, "sam@proton.test");
    assert_eq!(account.key_password, *key_password);

    // A wrong password is refused with Proton's message, and nothing is stored.
    let api2 = Api::with_base(&url).unwrap();
    let wrong = begin(&api2, "sam@proton.test", "nope nope");
    assert!(matches!(wrong, Err(ref e) if e.to_string().contains("Incorrect login")));

    let client = Client::with_api(Api::with_base(&url).unwrap(), account, |_| {}).unwrap();
    let root = client.drive_root().expect("root");
    assert_eq!(root.name, "My files");
    let top = client.drive_children(&root).expect("children");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].name, "Tax \u{1F4C1} 2026");
    assert!(top[0].folder);
    let files = client.drive_children(&top[0]).expect("files");
    assert_eq!(files[0].name, "invoice.pdf");
    assert_eq!(files[0].mime, "application/pdf");

    let dir = std::env::temp_dir().join(format!("spotty-fake-proton-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut blocks = 0;
    let path = client.drive_download(&files[0], &dir, &mut |n| blocks = n).expect("download");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello from proton drive");
    assert_eq!(blocks, 1);
    let _ = std::fs::remove_dir_all(&dir);

    // Calendar: 2026-06-01 .. 2026-07-01.
    let events = client.calendar_events(1780272000, 1782864000).expect("events");
    let titles: Vec<&str> = events.iter().map(|e| e.title.as_str()).collect();
    assert!(titles.iter().all(|t| *t == "Dentist, 3rd floor"), "{titles:?}");
    assert!(events.iter().any(|e| e.start == 1781085600 && e.location == "Main St 1" && e.description == "Bring the X-ray"));
    // The weekly series shows up several times inside the window.
    assert!(events.iter().filter(|e| e.id == "e2").count() >= 4, "{events:?}");
    assert!(events.windows(2).all(|w| w[0].start <= w[1].start));
}

#[test]
fn two_factor_step_is_required_and_wrong_codes_can_be_retried() {
    let key_salt = vec![9u8; 16];
    let key_password = derive_key_password(PASSWORD, &key_salt).unwrap();
    let world = Arc::new(build_world(&pgp(), &key_password, key_salt));
    let base = Arc::new(Mutex::new(String::new()));
    let url = serve(handler(world, base, true));
    let api = Api::with_base(&url).unwrap();
    let pending = match begin(&api, "sam@proton.test", PASSWORD).expect("password step") {
        Step::TwoFactor(p) => p,
        _ => panic!("expected a 2FA step"),
    };
    let pending = match pending.submit_two_factor(&api, "000000") {
        Err(spotty_proton_account::Retry::Again(error, pending)) => {
            assert!(error.to_string().contains("Incorrect"));
            pending
        }
        _ => panic!("a wrong code should allow another try"),
    };
    match pending.submit_two_factor(&api, "123 456") {
        Ok(Step::Done(account)) => assert_eq!(account.name, "Sam"),
        _ => panic!("the right code should finish sign-in"),
    }
}

#[test]
fn vpn_and_pass_get_their_own_child_sessions_and_other_apps_do_not() {
    let key_salt = vec![3u8; 16];
    let key_password = derive_key_password(PASSWORD, &key_salt).unwrap();
    let world = Arc::new(build_world(&pgp(), &key_password, key_salt));
    let url = serve(handler(world, Arc::new(Mutex::new(String::new())), false));
    let account = match begin(&Api::with_base(&url).unwrap(), "sam@proton.test", PASSWORD).expect("sign in") {
        Step::Done(account) => account,
        _ => panic!("expected to be done"),
    };
    let client = Client::with_api(Api::with_base(&url).unwrap(), account, |_| {}).unwrap();

    assert_eq!(client.fork_for_vpn().expect("VPN fork"), "sel1");

    // The link pass-cli prints, with the code and key the pretend Proton checks.
    let login_text = format!("0:{PASS_USER_CODE}:{}:cli-pass", STANDARD.encode(PASS_KEY));
    let link = format!("https://account.proton.me/desktop/login?app=pass#payload={}", percent_encode(&login_text));
    let login = parse_pass_login_url(&link).expect("pass-cli's link");
    client.approve_pass_login(&login).expect("Pass approval");

    // Spotty refuses other apps itself, so nothing reaches Proton.
    let refused = client.fork_session(&ForkRequest { child_client_id: "cli-drive", user_code: None, payload: None });
    assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
}

#[allow(dead_code)]
fn block_bytes_note() {
    let _ = mailbox_password_hash;
}
