//! SMTP AUTH and anonymous delivery, end to end over real SMTP.

mod common;

use common::*;
use lettre::transport::smtp::authentication::Mechanism;

fn assert_rejected(result: Result<(), String>, code: &str) {
    let error = result.expect_err("the server should have refused");
    assert!(error.starts_with(code), "expected {code}, got: {error}");
}

#[tokio::test]
async fn auth_only_rejects_anonymous_mail() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;

    let result = send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("s", "b"),
    )
    .await;
    assert_rejected(result, "530");
    assert!(telegram.requests().is_empty());
}

#[tokio::test]
async fn auth_only_accepts_plain() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;

    send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Plain,
        message_from(USER_ADDRESS, "plain", "b"),
    )
    .await
    .unwrap();
    let messages = telegram.messages();
    assert_eq!(messages.len(), 2);
    assert!(
        messages[0]
            .field("text")
            .unwrap()
            .contains("Subject: plain")
    );
}

#[tokio::test]
async fn auth_only_accepts_login() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;

    send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Login,
        message_from(USER_ADDRESS, "login", "b"),
    )
    .await
    .unwrap();
    let messages = telegram.messages();
    assert_eq!(messages.len(), 2);
    assert!(
        messages[0]
            .field("text")
            .unwrap()
            .contains("Subject: login")
    );
}

#[tokio::test]
async fn auth_only_rejects_wrong_password_and_unknown_user() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;

    for mechanism in [Mechanism::Plain, Mechanism::Login] {
        let result = send(
            server.addr,
            Some((USER, "wrong")),
            mechanism,
            simple_message("s", "b"),
        )
        .await;
        assert_rejected(result, "535");
        let result = send(
            server.addr,
            Some(("mallory", PASSWORD)),
            mechanism,
            simple_message("s", "b"),
        )
        .await;
        assert_rejected(result, "535");
    }
    assert!(telegram.requests().is_empty());
}

#[tokio::test]
async fn auth_and_anonymous_accepts_both() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, true)).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("anonymous", "b"),
    )
    .await
    .unwrap();
    send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Plain,
        message_from(USER_ADDRESS, "plain", "b"),
    )
    .await
    .unwrap();
    send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Login,
        message_from(USER_ADDRESS, "login", "b"),
    )
    .await
    .unwrap();
    // A client that tries to authenticate still has to get it right.
    let result = send(
        server.addr,
        Some((USER, "wrong")),
        Mechanism::Plain,
        simple_message("bad", "b"),
    )
    .await;
    assert_rejected(result, "535");

    let subjects: Vec<String> = telegram
        .messages()
        .iter()
        .map(|m| m.field("text").unwrap().lines().nth(2).unwrap().to_string())
        .collect();
    assert_eq!(
        subjects,
        [
            "Subject: anonymous",
            "Subject: anonymous",
            "Subject: plain",
            "Subject: plain",
            "Subject: login",
            "Subject: login"
        ]
    );
}

#[tokio::test]
async fn anonymous_only_does_not_offer_auth() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    let mut client = RawClient::connect(server.addr).await;
    let (code, ehlo) = client.cmd("EHLO client.test").await;
    assert_eq!(code, 250);
    assert!(!ehlo.contains("AUTH"), "{ehlo}");
    let (code, _) = client
        .cmd(&format!("AUTH PLAIN {}", b64("\0alice\0x")))
        .await;
    assert_eq!(code, 502);
}

#[tokio::test]
async fn auth_is_not_offered_outside_plaintext_auth_networks() {
    let telegram = MockTelegram::start().await;
    let mut config = auth_config(&telegram.prefix, false);
    // The test client connects from 127.0.0.1, which is not in here.
    config.smtp.plaintext_auth_networks = vec!["192.0.2.0/24".parse().unwrap()];
    let server = TestServer::start(config).await;

    let mut client = RawClient::connect(server.addr).await;
    let (code, ehlo) = client.cmd("EHLO client.test").await;
    assert_eq!(code, 250);
    assert!(!ehlo.contains("AUTH"), "{ehlo}");
    let (code, text) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(
        (code, text.as_str()),
        (
            538,
            "5.7.11 Encryption required for requested authentication mechanism"
        )
    );
    let (code, _) = client.cmd("MAIL FROM:<from@test>").await;
    assert_eq!(code, 530);

    // lettre refuses to send credentials the server does not ask for.
    let result = send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Plain,
        simple_message("s", "b"),
    )
    .await;
    assert!(result.is_err());
    assert!(telegram.requests().is_empty());
}

#[tokio::test]
async fn auth_protocol_details() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;
    let mut client = RawClient::connect(server.addr).await;

    // AUTH needs EHLO.
    let (code, _) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(code, 503);

    let (code, ehlo) = client.cmd("EHLO client.test").await;
    assert_eq!(code, 250);
    assert!(ehlo.lines().any(|l| l == "AUTH PLAIN LOGIN"), "{ehlo}");

    // Unknown mechanism, cancelled exchange, bad base64.
    assert_eq!(client.cmd("AUTH CRAM-MD5").await.0, 504);
    assert_eq!(client.cmd("AUTH PLAIN").await.0, 334);
    assert_eq!(client.cmd("*").await.0, 501);
    assert_eq!(client.cmd("AUTH PLAIN !!!notbase64").await.0, 501);

    // An authorization identity other than the user is refused.
    let (code, _) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("bob\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(code, 535);

    // PLAIN without an initial response: 334, then the credentials.
    assert_eq!(client.cmd("AUTH PLAIN").await, (334, String::new()));
    let (code, text) = client
        .cmd(&b64(&format!("{USER}\0{USER}\0{PASSWORD}")))
        .await;
    assert_eq!(
        (code, text.as_str()),
        (235, "2.7.0 Authentication successful")
    );

    // Only once per session.
    assert_eq!(client.cmd("AUTH LOGIN").await.0, 503);
    assert_eq!(client.cmd("MAIL FROM:<alice@test>").await.0, 250);
}

#[tokio::test]
async fn an_authenticated_client_sends_as_itself() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, true)).await;

    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;
    let (code, _) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(code, 235);
    // Someone else's address, the null sender, no domain: all refused, and
    // the session can go on to try again.
    for sender in ["<mallory@test>", "<>", "<alice>", "<alice@>"] {
        let (code, text) = client.cmd(&format!("MAIL FROM:{sender}")).await;
        assert_eq!(code, 553, "{sender}: {text}");
    }
    // Its own name at any domain, in any case.
    assert_eq!(client.cmd("MAIL FROM:<Alice@elsewhere.test>").await.0, 250);

    // Anonymous clients are not checked.
    let mut anonymous = RawClient::connect(server.addr).await;
    anonymous.cmd("EHLO client.test").await;
    assert_eq!(anonymous.cmd("MAIL FROM:<mallory@test>").await.0, 250);

    // End to end: a mismatched sender is refused and nothing is forwarded.
    let result = send(
        server.addr,
        Some((USER, PASSWORD)),
        Mechanism::Plain,
        simple_message("s", "b"),
    )
    .await;
    assert_rejected(result, "553");
    assert!(telegram.requests().is_empty());
}

#[tokio::test]
async fn auth_is_refused_inside_a_transaction() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, true)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    assert_eq!(client.cmd("MAIL FROM:<from@test>").await.0, 250);
    let (code, _) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(code, 503);
    assert_eq!(client.cmd("RSET").await.0, 250);
    let (code, _) = client
        .cmd(&format!(
            "AUTH PLAIN {}",
            b64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert_eq!(code, 235);
}

#[tokio::test]
async fn auth_login_with_initial_response() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    let (code, prompt) = client.cmd(&format!("AUTH LOGIN {}", b64(USER))).await;
    assert_eq!((code, prompt.as_str()), (334, "UGFzc3dvcmQ6"));
    let (code, _) = client.cmd(&b64(PASSWORD)).await;
    assert_eq!(code, 235);
}

#[tokio::test]
async fn three_failed_attempts_close_the_connection() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(auth_config(&telegram.prefix, false)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    let bad = format!("AUTH PLAIN {}", b64(&format!("\0{USER}\0nope")));
    assert_eq!(client.cmd(&bad).await.0, 535);
    assert_eq!(client.cmd(&bad).await.0, 535);
    assert_eq!(client.cmd(&bad).await.0, 421);
    assert!(client.is_closed().await);
}
