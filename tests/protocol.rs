//! SMTP protocol behaviour at the line level.

mod common;

use std::time::Duration;

use common::*;

#[tokio::test]
async fn greeting_and_ehlo_extensions() {
    let telegram = MockTelegram::start().await;
    let mut config = auth_config(&telegram.prefix, true);
    config.smtp.max_message_size = 12345;
    let server = TestServer::start(config).await;

    let (mut client, code, greeting) = RawClient::connect_raw(server.addr).await;
    assert_eq!(
        (code, greeting.as_str()),
        (220, "testhost ESMTP smtp_to_telegram")
    );
    let (code, ehlo) = client.cmd("EHLO client.test").await;
    assert_eq!(code, 250);
    assert_eq!(
        ehlo,
        "testhost Hello client.test\nSIZE 12345\n8BITMIME\nPIPELINING\nENHANCEDSTATUSCODES\nAUTH PLAIN LOGIN\nHELP"
    );
    assert_eq!(
        client.cmd("HELO client.test").await,
        (250, "testhost Hello client.test".to_string())
    );
    assert_eq!(client.cmd("EHLO").await.0, 501);
    assert_eq!(client.cmd("NOOP").await.0, 250);
    assert_eq!(client.cmd("VRFY someone").await.0, 252);
    assert_eq!(client.cmd("HELP").await.0, 214);
    assert_eq!(client.cmd("STARTTLS").await.0, 500);
    assert_eq!(client.cmd("QUIT").await.0, 221);
    assert!(client.is_closed().await);
}

#[tokio::test]
async fn command_sequence_errors() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    assert_eq!(client.cmd("RCPT TO:<to@test>").await.0, 503);
    assert_eq!(client.cmd("DATA").await.0, 503);
    assert_eq!(client.cmd("MAIL <from@test>").await.0, 501);
    assert_eq!(client.cmd("MAIL FROM:<from@test>").await.0, 250);
    assert_eq!(client.cmd("MAIL FROM:<from@test>").await.0, 503);
    assert_eq!(client.cmd("DATA").await.0, 503);
    assert_eq!(client.cmd("RCPT TO:<>").await.0, 501);
    assert_eq!(client.cmd("RCPT TO:<bad address@test>").await.0, 501);
    assert_eq!(client.cmd("RSET").await.0, 250);
    assert_eq!(client.cmd("RCPT TO:<to@test>").await.0, 503);
    assert!(telegram.requests().is_empty());
}

#[tokio::test]
async fn pipelined_transaction() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    client
        .write(b"MAIL FROM:<from@test>\r\nRCPT TO:<to@test>\r\nRCPT TO:<other@test>\r\nDATA\r\n")
        .await;
    assert_eq!(client.reply().await.0, 250);
    assert_eq!(client.reply().await.0, 250);
    assert_eq!(client.reply().await.0, 250);
    assert_eq!(client.reply().await.0, 354);
    client
        .write(b"Subject: piped\r\n\r\nbody\r\n.\r\nQUIT\r\n")
        .await;
    assert_eq!(client.reply().await.0, 250);
    assert_eq!(client.reply().await.0, 221);
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("From: from@test\nTo: to@test, other@test\nSubject: piped\n\nbody")
    );
}

#[tokio::test]
async fn dot_stuffing_is_undone() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;
    client.cmd("MAIL FROM:<from@test>").await;
    client.cmd("RCPT TO:<to@test>").await;
    assert_eq!(client.cmd("DATA").await.0, 354);
    client
        .write(b"Subject: dots\r\n\r\n..leading dot\r\n...\r\nend\r\n")
        .await;
    assert_eq!(client.cmd(".").await.0, 250);
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("From: from@test\nTo: to@test\nSubject: dots\n\n.leading dot\n..\nend")
    );
}

#[tokio::test]
async fn size_limit() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.smtp.max_message_size = 1000;
    let server = TestServer::start(config).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;

    // Declared too large up front.
    assert_eq!(client.cmd("MAIL FROM:<from@test> SIZE=5000").await.0, 552);
    assert_eq!(
        client
            .cmd("MAIL FROM:<from@test> SIZE=500 BODY=8BITMIME")
            .await
            .0,
        250
    );
    client.cmd("RCPT TO:<to@test>").await;
    assert_eq!(client.cmd("DATA").await.0, 354);
    // Too large in fact: many lines, then one huge line.
    for _ in 0..20 {
        client.write(&[b'x'; 98]).await;
        client.write(b"\r\n").await;
    }
    client.write(&vec![b'y'; 100_000]).await;
    client.write(b"\r\n").await;
    let (code, text) = client.cmd(".").await;
    assert_eq!(
        (code, text.as_str()),
        (552, "5.3.4 Message size exceeds fixed maximum message size")
    );

    // The session goes on, and a small message still works.
    assert_eq!(client.cmd("MAIL FROM:<from@test>").await.0, 250);
    client.cmd("RCPT TO:<to@test>").await;
    assert_eq!(client.cmd("DATA").await.0, 354);
    client.write(b"Subject: small\r\n\r\nok\r\n").await;
    assert_eq!(client.cmd(".").await.0, 250);
    assert_eq!(telegram.messages().len(), 2);
}

#[tokio::test]
async fn recipient_limit() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;
    client.cmd("EHLO client.test").await;
    client.cmd("MAIL FROM:<from@test>").await;
    for i in 0..100 {
        assert_eq!(client.cmd(&format!("RCPT TO:<r{i}@test>")).await.0, 250);
    }
    assert_eq!(client.cmd("RCPT TO:<one-too-many@test>").await.0, 452);
}

#[tokio::test]
async fn too_many_unrecognized_commands_close_the_connection() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;

    for _ in 0..4 {
        assert_eq!(client.cmd("BOGUS").await.0, 500);
    }
    assert_eq!(client.cmd("BOGUS").await.0, 554);
    assert!(client.is_closed().await);
}

#[tokio::test]
async fn overlong_command_line_is_refused() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let mut client = RawClient::connect(server.addr).await;

    let long = format!("NOOP {}", "x".repeat(10_000));
    assert_eq!(client.cmd(&long).await.0, 500);
    assert_eq!(client.cmd("NOOP").await.0, 250);
}

#[tokio::test]
async fn connection_limit() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.smtp.max_connections = 1;
    let server = TestServer::start(config).await;

    let mut first = RawClient::connect(server.addr).await;
    let (_second, code, text) = RawClient::connect_raw(server.addr).await;
    assert_eq!(code, 421, "{text}");
    assert_eq!(first.cmd("NOOP").await.0, 250);
    assert_eq!(first.cmd("QUIT").await.0, 221);
    assert!(first.is_closed().await);

    // The slot is free again.
    let mut third = None;
    for _ in 0..50 {
        let (client, code, _) = RawClient::connect_raw(server.addr).await;
        if code == 220 {
            third = Some(client);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(third.expect("a slot frees up").cmd("NOOP").await.0, 250);
}

#[tokio::test]
async fn idle_timeout_closes_the_connection() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.smtp.timeout = Duration::from_millis(300);
    let server = TestServer::start(config).await;
    let mut client = RawClient::connect(server.addr).await;

    let (code, text) = client.reply().await;
    assert_eq!(
        (code, text.as_str()),
        (421, "4.4.2 Idle timeout, closing connection")
    );
    assert!(client.is_closed().await);
}

#[tokio::test]
async fn graceful_shutdown_tells_idle_clients() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;
    let addr = server.addr;
    let mut client = RawClient::connect(addr).await;
    client.cmd("EHLO client.test").await;

    server.shutdown().await.unwrap();
    let (code, text) = client.reply().await;
    assert_eq!((code, text.as_str()), (421, "4.3.2 Service shutting down"));
    assert!(client.is_closed().await);
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
