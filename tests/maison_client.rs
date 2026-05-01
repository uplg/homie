use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_json, header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use homie::maison::MaisonClient;

const FAKE_JWT: &str = "fake.jwt.token";
const FAKE_JWT_REFRESHED: &str = "refreshed.jwt.token";

async fn mount_login_ok(server: &MockServer, jwt: &str) {
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .and(body_json(
            json!({"username": "alice", "password": "secret"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header(
                    "set-cookie",
                    format!("maison_session={jwt}; Path=/; HttpOnly; Max-Age=900").as_str(),
                )
                .append_header(
                    "set-cookie",
                    "maison_refresh=uuid-1; Path=/api/auth; HttpOnly; Max-Age=604800",
                )
                .set_body_json(json!({
                    "success": true,
                    "user": {"id": "1", "username": "alice", "role": "admin"}
                })),
        )
        .mount(server)
        .await;
}

fn client(server_url: &str) -> MaisonClient {
    let base = Url::parse(server_url).expect("server url");
    MaisonClient::new(&base, "alice".into(), "secret".into()).expect("client")
}

#[tokio::test]
async fn login_extracts_jwt_and_authorizes_subsequent_calls() {
    let server = MockServer::start().await;
    mount_login_ok(&server, FAKE_JWT).await;

    Mock::given(method("POST"))
        .and(path("/api/zigbee/lamps/lamp-1/power"))
        .and(header(
            "authorization",
            format!("Bearer {FAKE_JWT}").as_str(),
        ))
        .and(body_json(json!({"enabled": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "state": {},
            "message": "Zigbee lamp power updated"
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let resp = client
        .set_lamp_power("lamp-1", true)
        .await
        .expect("set_lamp_power");
    assert!(resp.success);
    assert_eq!(resp.message, "Zigbee lamp power updated");
}

#[tokio::test]
async fn retry_after_401_reissues_login_and_replays_request() {
    let server = MockServer::start().await;

    // Login responds with two different JWTs across two calls so we can assert the
    // second attempt uses the refreshed token.
    let login_call_count = Arc::new(AtomicUsize::new(0));
    let counter_for_responder = Arc::clone(&login_call_count);
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .respond_with(move |_req: &Request| {
            let attempt = counter_for_responder.fetch_add(1, Ordering::SeqCst);
            let jwt = if attempt == 0 {
                FAKE_JWT
            } else {
                FAKE_JWT_REFRESHED
            };
            ResponseTemplate::new(200)
                .append_header(
                    "set-cookie",
                    format!("maison_session={jwt}; Path=/; HttpOnly").as_str(),
                )
                .set_body_json(json!({"success": true}))
        })
        .mount(&server)
        .await;

    // First call to brightness with old token returns 401.
    Mock::given(method("POST"))
        .and(path("/api/zigbee/lamps/lamp-1/brightness"))
        .and(header(
            "authorization",
            format!("Bearer {FAKE_JWT}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "expired"})))
        .expect(1)
        .mount(&server)
        .await;

    // Retry with refreshed token succeeds.
    Mock::given(method("POST"))
        .and(path("/api/zigbee/lamps/lamp-1/brightness"))
        .and(header(
            "authorization",
            format!("Bearer {FAKE_JWT_REFRESHED}").as_str(),
        ))
        .and(body_json(json!({"brightness": 120})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "state": {},
            "message": "Zigbee lamp brightness updated"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let resp = client
        .set_lamp_brightness("lamp-1", 120)
        .await
        .expect("set_lamp_brightness");
    assert!(resp.success);
    assert_eq!(login_call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn mitsubishi_send_serializes_camel_case_local_ip() {
    let server = MockServer::start().await;
    mount_login_ok(&server, FAKE_JWT).await;

    Mock::given(method("POST"))
        .and(path("/api/broadlink/mitsubishi/send"))
        .and(header_exists("authorization"))
        .and(body_json(json!({
            "host": "192.168.1.42",
            "localIp": "192.168.1.10",
            "command": "power_on_cool_22",
            "model": "msz",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {"host": "192.168.1.42"},
            "message": "Mitsubishi command sent via Broadlink device 192.168.1.42"
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let resp = client
        .send_mitsubishi_command(
            "192.168.1.42",
            "power_on_cool_22",
            Some("msz"),
            Some("192.168.1.10"),
        )
        .await
        .expect("ac send");
    assert!(resp.success);
    assert!(resp.message.contains("Mitsubishi"));
}

#[tokio::test]
async fn mitsubishi_send_omits_optional_fields_when_none() {
    let server = MockServer::start().await;
    mount_login_ok(&server, FAKE_JWT).await;

    // Body must be exactly {"host","command"} — no localIp / model keys.
    Mock::given(method("POST"))
        .and(path("/api/broadlink/mitsubishi/send"))
        .and(body_json(json!({
            "host": "192.168.1.42",
            "command": "power_off",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {"host": "192.168.1.42"},
            "message": "Mitsubishi command sent"
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    client
        .send_mitsubishi_command("192.168.1.42", "power_off", None, None)
        .await
        .expect("ac send");
}

#[tokio::test]
async fn feeder_feed_posts_portion_payload() {
    let server = MockServer::start().await;
    mount_login_ok(&server, FAKE_JWT).await;

    Mock::given(method("POST"))
        .and(path("/api/devices/bf123/feeder/feed"))
        .and(header_exists("authorization"))
        .and(body_json(json!({"portion": 1})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "message": "Manual feed command sent to Apollo with portions: 1",
            "device": {"id": "bf123", "name": "Apollo"}
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let resp = client.feeder_feed("bf123", 1).await.expect("feeder feed");
    assert!(resp.success);
    assert!(resp.message.contains("Apollo"));
}

#[tokio::test]
async fn list_zigbee_lamps_decodes_array() {
    let server = MockServer::start().await;
    mount_login_ok(&server, FAKE_JWT).await;

    Mock::given(method("GET"))
        .and(path("/api/zigbee/lamps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "lamps": [
                {"id": "lamp-1", "name": "Salon", "connected": true, "reachable": true},
                {"id": "lamp-2", "name": "Bureau", "connected": true, "reachable": false}
            ],
            "total": 2,
            "connected": 2,
            "reachable": 1,
            "message": "Zigbee lamps list retrieved"
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let lamps = client.list_zigbee_lamps().await.expect("list lamps");
    assert_eq!(lamps.len(), 2);
    assert_eq!(lamps[0].id, "lamp-1");
    assert_eq!(lamps[1].name.as_deref(), Some("Bureau"));
}

#[tokio::test]
async fn maison_error_when_login_returns_no_session_cookie() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let err = client
        .login()
        .await
        .expect_err("login should fail without cookie");
    let msg = err.to_string();
    assert!(msg.contains("missing maison_session"), "got: {msg}");
}

// Sanity check: not used elsewhere but ensures `Value` import is consumed
// (silences unused-import warnings when the rest of the file evolves).
#[allow(dead_code)]
fn _ensure_value_used() -> Value {
    Value::Null
}
