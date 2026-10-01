//! What reaches Telegram: template, chats, parse modes, attachments, long
//! messages and error handling, end to end over real SMTP.

mod common;

use common::*;
use lettre::Message;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Mechanism;
use smtp_to_telegram::ParseMode;

const DEFAULT_TEXT: &str = "From: from@test\nTo: to@test\nSubject: Test subj\n\nText body";

/// Sends raw message data over a line-level client, returning the reply to
/// the end of DATA.
async fn send_raw(addr: std::net::SocketAddr, data: &str) -> (u16, String) {
    let mut client = RawClient::connect(addr).await;
    assert_eq!(client.cmd("EHLO client.test").await.0, 250);
    assert_eq!(client.cmd("MAIL FROM:<from@test>").await.0, 250);
    assert_eq!(client.cmd("RCPT TO:<to@test>").await.0, 250);
    assert_eq!(client.cmd("DATA").await.0, 354);
    client.write(data.as_bytes()).await;
    client.cmd(".").await
}

fn message_with_attachments() -> Message {
    let jpeg = ContentType::parse("image/jpeg").unwrap();
    Message::builder()
        .from("from@test".parse().unwrap())
        .to("to@test".parse().unwrap())
        .subject("Test subj")
        .multipart(
            MultiPart::mixed()
                .multipart(
                    MultiPart::related()
                        .multipart(MultiPart::alternative_plain_html(
                            "Text body".to_string(),
                            "<p>HTML body</p>".to_string(),
                        ))
                        .singlepart(
                            Attachment::new_inline_with_name(
                                "inline.jpg".into(),
                                "inline.jpg".into(),
                            )
                            .body(b"JPG".to_vec(), jpeg.clone()),
                        ),
                )
                .singlepart(
                    Attachment::new("hey.txt".into()).body(b"hi".to_vec(), ContentType::TEXT_PLAIN),
                )
                .singlepart(Attachment::new("attachment.jpg".into()).body(b"JPG".to_vec(), jpeg)),
        )
        .unwrap()
}

#[tokio::test]
async fn default_template_goes_to_every_chat() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("Test subj", "Text body"),
    )
    .await
    .unwrap();

    let messages = telegram.messages();
    assert_eq!(messages.len(), 2);
    for (message, chat) in messages.iter().zip(CHAT_IDS) {
        assert_eq!(message.token, TOKEN);
        assert_eq!(message.field("chat_id"), Some(chat));
        assert_eq!(message.field("text"), Some(DEFAULT_TEXT));
        assert_eq!(message.field("parse_mode"), None);
        assert_eq!(
            message.field("link_preview_options"),
            Some(r#"{"is_disabled":true}"#)
        );
    }
    assert!(telegram.files().is_empty());
}

#[tokio::test]
async fn custom_template() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.template = "Subject: {subject}\\n\\n{body}".to_string();
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("Test subj", "Text body"),
    )
    .await
    .unwrap();
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("Subject: Test subj\n\nText body")
    );
}

#[tokio::test]
async fn several_recipients_and_null_sender() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    let mut client = RawClient::connect(server.addr).await;
    assert_eq!(client.cmd("HELO client.test").await.0, 250);
    assert_eq!(client.cmd("MAIL FROM:<>").await.0, 250);
    assert_eq!(client.cmd("RCPT TO:<a@test>").await.0, 250);
    assert_eq!(client.cmd("RCPT TO:<b@test>").await.0, 250);
    assert_eq!(client.cmd("DATA").await.0, 354);
    client.write(b"Subject: bounce\r\n\r\nbody\r\n").await;
    assert_eq!(client.cmd(".").await.0, 250);

    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("From: \nTo: a@test, b@test\nSubject: bounce\n\nbody")
    );
}

#[tokio::test]
async fn markdown_v2_escapes_substituted_values() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.template = "*{subject}*\\n_{from}_\\n\\n{body}".to_string();
    config.format.parse_mode = ParseMode::MarkdownV2;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("WAN down (eth1)!", "Ping 1.1.1.1 failed: 100% loss [x]"),
    )
    .await
    .unwrap();

    let message = &telegram.messages()[0];
    assert_eq!(message.field("parse_mode"), Some("MarkdownV2"));
    assert_eq!(
        message.field("text"),
        Some(
            "*WAN down \\(eth1\\)\\!*\n_from@test_\n\nPing 1\\.1\\.1\\.1 failed: 100% loss \\[x\\]"
        )
    );
}

#[tokio::test]
async fn html_parse_mode_escapes_substituted_values() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.template = "<b>{subject}</b>\\n{from} -> {to}\\n{body}".to_string();
    config.format.parse_mode = ParseMode::Html;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("a < b", "x & y"),
    )
    .await
    .unwrap();
    let message = &telegram.messages()[0];
    assert_eq!(message.field("parse_mode"), Some("HTML"));
    assert_eq!(
        message.field("text"),
        Some("<b>a &lt; b</b>\nfrom@test -> to@test\nx &amp; y")
    );
}

#[tokio::test]
async fn encoded_subject_and_body_are_decoded() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    let (code, _) = send_raw(
        server.addr,
        "Subject: =?UTF-8?B?8J+Yjg==?=\r\nContent-Type: text/plain; charset=UTF-8\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\r\n=F0=9F=92=A9\r\n",
    )
    .await;
    assert_eq!(code, 250);
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("From: from@test\nTo: to@test\nSubject: 😎\n\n💩")
    );
}

#[tokio::test]
async fn html_only_body_is_converted_to_text() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    let message = Message::builder()
        .from("from@test".parse().unwrap())
        .to("to@test".parse().unwrap())
        .subject("Test subj")
        .singlepart(SinglePart::html("<p>Text <b>body</b></p>".to_string()))
        .unwrap();
    send(server.addr, None, Mechanism::Plain, message)
        .await
        .unwrap();
    assert_eq!(telegram.messages()[0].field("text"), Some(DEFAULT_TEXT));
}

#[tokio::test]
async fn attachments_are_forwarded_as_replies() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.attachment_max_size = 1024;
    config.format.attachment_max_photo_size = 1024;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap();

    let messages = telegram.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].field("text"),
        Some(
            "From: from@test\nTo: to@test\nSubject: Test subj\n\nText body\n\nAttachments:\n\
             - 🔗 inline.jpg (image/jpeg) 3B, sending...\n\
             - 📎 hey.txt (text/plain) 2B, sending...\n\
             - 📎 attachment.jpg (image/jpeg) 3B, sending..."
        )
    );

    // Requests run in order: message, its files, then the next chat.
    let requests = telegram.requests();
    let methods: Vec<&str> = requests.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(
        methods,
        [
            "sendMessage",
            "sendPhoto",
            "sendDocument",
            "sendPhoto",
            "sendMessage",
            "sendPhoto",
            "sendDocument",
            "sendPhoto"
        ]
    );
    let expected = [
        ("photo", "inline.jpg", "image/jpeg", &b"JPG"[..]),
        ("document", "hey.txt", "text/plain", &b"hi"[..]),
        ("photo", "attachment.jpg", "image/jpeg", &b"JPG"[..]),
    ];
    for chat in 0..2 {
        let message = &requests[chat * 4];
        let message_id: i64 = 1001 + (chat as i64) * 4;
        assert_eq!(message.field("chat_id"), Some(CHAT_IDS[chat]));
        for (i, (field, name, content_type, content)) in expected.iter().enumerate() {
            let request = &requests[chat * 4 + 1 + i];
            assert_eq!(request.field("chat_id"), Some(CHAT_IDS[chat]));
            assert_eq!(request.field("caption"), Some(*name));
            assert_eq!(request.field("disable_notification"), Some("true"));
            assert_eq!(
                request.field("reply_parameters"),
                Some(
                    format!(r#"{{"allow_sending_without_reply":true,"message_id":{message_id}}}"#)
                        .as_str()
                )
            );
            let file = request.file.as_ref().unwrap();
            assert_eq!(file.field, *field);
            assert_eq!(file.filename, *name);
            assert_eq!(file.content_type, *content_type);
            assert_eq!(file.content, *content);
        }
    }
}

#[tokio::test]
async fn attachments_over_the_limit_are_listed_but_not_sent() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.attachment_max_size = 0;
    config.format.attachment_max_photo_size = 0;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap();
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some(
            "From: from@test\nTo: to@test\nSubject: Test subj\n\nText body\n\nAttachments:\n\
             - 🔗 inline.jpg (image/jpeg) 3B, discarded\n\
             - 📎 hey.txt (text/plain) 2B, discarded\n\
             - 📎 attachment.jpg (image/jpeg) 3B, discarded"
        )
    );
    assert!(telegram.files().is_empty());
}

#[tokio::test]
async fn photo_larger_than_the_photo_limit_goes_as_a_document() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.telegram.chat_ids = vec!["42".to_string()];
    config.format.attachment_max_size = 1024;
    config.format.attachment_max_photo_size = 2;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap();
    let methods: Vec<String> = telegram.files().into_iter().map(|r| r.method).collect();
    assert_eq!(methods, ["sendDocument", "sendDocument", "sendDocument"]);
}

#[tokio::test]
async fn long_message_is_truncated_and_attached_in_full() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.format.message_length_to_send_as_file = 100;
    let server = TestServer::start(config).await;

    let body = "Hello_".repeat(60);
    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("Test subj", &body),
    )
    .await
    .unwrap();

    let messages = telegram.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0].field("text"),
        Some(
            "From: from@test\nTo: to@test\nSubject: Test subj\n\n\
             Hello_Hello_Hello_Hello_Hello_Hello_He\n\n[truncated]"
        )
    );
    let files = telegram.files();
    assert_eq!(files.len(), 2);
    for file in &files {
        assert_eq!(file.method, "sendDocument");
        assert_eq!(file.field("caption"), Some("Full message"));
        let upload = file.file.as_ref().unwrap();
        assert_eq!(upload.filename, "full_message.txt");
        assert_eq!(
            String::from_utf8(upload.content.clone()).unwrap(),
            format!("From: from@test\nTo: to@test\nSubject: Test subj\n\n{body}")
        );
    }
}

#[tokio::test]
async fn default_length_limit_keeps_messages_under_telegrams_4096() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.telegram.chat_ids = vec!["42".to_string()];
    let server = TestServer::start(config).await;

    // 5000 characters, a quarter of them emoji (two UTF-16 units each).
    let body: String = "abc📎".repeat(1250);
    send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("big", &body),
    )
    .await
    .unwrap();
    let text = telegram.messages()[0].field("text").unwrap().to_string();
    assert!(text.ends_with("\n\n[truncated]"));
    let utf16_len: usize = text.encode_utf16().count();
    assert!(utf16_len <= 4095, "{utf16_len}");
    assert!(utf16_len > 4000, "{utf16_len}");
    assert_eq!(
        telegram.files()[0].file.as_ref().unwrap().filename,
        "full_message.txt"
    );
}

#[tokio::test]
async fn telegram_error_is_a_temporary_failure_without_the_token() {
    let telegram = MockTelegram::start().await;
    telegram.fail("sendMessage");
    let server = TestServer::start(config(&telegram.prefix)).await;

    let (code, text) = send_raw(server.addr, "Subject: s\r\n\r\nb\r\n").await;
    assert_eq!(code, 451);
    assert!(
        text.contains("HTTP 400: Bad Request: mock failure"),
        "{text}"
    );
    assert!(!text.contains(TOKEN), "{text}");
    // The first chat failed, so the second was never tried.
    assert_eq!(telegram.messages().len(), 1);
}

#[tokio::test]
async fn telegram_unreachable_is_a_temporary_failure() {
    // Nothing listens on this port once the listener is dropped.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let server = TestServer::start(config(&format!("http://{closed}/"))).await;

    let (code, text) = send_raw(server.addr, "Subject: s\r\n\r\nb\r\n").await;
    assert_eq!(code, 451, "{text}");
    assert!(!text.contains(TOKEN), "{text}");

    let result = send(
        server.addr,
        None,
        Mechanism::Plain,
        simple_message("s", "b"),
    )
    .await;
    let error = result.unwrap_err();
    assert!(error.starts_with("451"), "{error}");
}

#[tokio::test]
async fn attachment_errors_are_ignored_by_default() {
    let telegram = MockTelegram::start().await;
    telegram.fail("sendDocument");
    let mut config = config(&telegram.prefix);
    config.format.attachment_max_size = 1024;
    config.format.attachment_max_photo_size = 1024;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap();
    assert_eq!(telegram.messages().len(), 2);
    // Both photos per chat still went out.
    let photos = telegram
        .files()
        .iter()
        .filter(|r| r.method == "sendPhoto")
        .count();
    assert_eq!(photos, 4);
}

#[tokio::test]
async fn attachment_errors_fail_the_mail_when_respected() {
    let telegram = MockTelegram::start().await;
    telegram.fail("sendDocument");
    let mut config = config(&telegram.prefix);
    config.format.attachment_max_size = 1024;
    config.format.attachment_max_photo_size = 1024;
    config.telegram.attachment_respect_errors = true;
    let server = TestServer::start(config).await;

    let error = send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap_err();
    assert!(error.starts_with("451"), "{error}");
}

#[tokio::test]
async fn extra_params_reach_every_request() {
    let telegram = MockTelegram::start().await;
    let mut config = config(&telegram.prefix);
    config.telegram.chat_ids = vec!["42".to_string()];
    config.telegram.extra_params = vec![("message_thread_id".to_string(), "7".to_string())];
    config.format.attachment_max_size = 1024;
    config.format.attachment_max_photo_size = 1024;
    let server = TestServer::start(config).await;

    send(
        server.addr,
        None,
        Mechanism::Plain,
        message_with_attachments(),
    )
    .await
    .unwrap();
    let requests = telegram.requests();
    assert_eq!(requests.len(), 4);
    for request in requests {
        assert_eq!(
            request.field("message_thread_id"),
            Some("7"),
            "{}",
            request.method
        );
    }
}

#[tokio::test]
async fn unparseable_data_is_forwarded_raw() {
    let telegram = MockTelegram::start().await;
    let server = TestServer::start(config(&telegram.prefix)).await;

    let (code, _) = send_raw(server.addr, "hi\r\n").await;
    assert_eq!(code, 250);
    assert_eq!(
        telegram.messages()[0].field("text"),
        Some("From: from@test\nTo: to@test\nSubject: \n\nhi")
    );
}
