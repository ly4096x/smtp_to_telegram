//! Turns a received email into a Telegram message plus attachments.
//!
//! Rules (kept from the Go version unless noted):
//! - `{body}` is the text/plain body. A message with only an HTML body gets
//!   it converted to text. A text/plain part marked as an attachment but
//!   without a file name (what GNU mailx sends) is used as the body when
//!   there is no other. If the data cannot be parsed as a message at all, the
//!   raw data is the body.
//! - Every other part is an attachment, listed in `{attachments_details}`
//!   and forwarded as a photo (JPEG/PNG) or a document within size limits.
//! - A message longer than `message_length_to_send_as_file` is truncated and
//!   the full text is attached as `full_message.txt`.

use std::borrow::Cow;

use mail_parser::{MessageParser, MimeHeaders, PartType};

use crate::config::{FormatConfig, ParseMode};
use crate::size::human_size;

/// Appended to a truncated body.
pub const TRUNCATION_MARKER: &str = "\n\n[truncated]";
pub const FULL_MESSAGE_FILENAME: &str = "full_message.txt";
pub const FULL_MESSAGE_CAPTION: &str = "Full message";

/// What the SMTP session received.
#[derive(Debug, Clone, Copy)]
pub struct Envelope<'a> {
    pub mail_from: &'a str,
    pub rcpt_to: &'a [String],
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Photo,
    Document,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub kind: AttachmentKind,
    /// Name of the uploaded file (no directory part).
    pub filename: String,
    pub caption: String,
    pub content_type: String,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedEmail {
    pub text: String,
    /// `None` when the text must be sent as plain text, either because no
    /// parse mode is configured or because the message had to be cut
    /// without regard to markup.
    pub parse_mode: Option<ParseMode>,
    pub attachments: Vec<Attachment>,
    /// Set when the full text did not fit `attachment_max_size` and so
    /// could not be attached after truncation.
    pub full_text_dropped: bool,
}

struct Values<'a> {
    from: &'a str,
    to: &'a str,
    subject: &'a str,
    attachments_details: &'a str,
}

/// Renders the template in one pass: each placeholder is replaced once and
/// the substituted text is never scanned again, so a body containing
/// `{subject}` stays as it is. `\n` (backslash, n) in the template is a
/// newline.
fn render(template: &str, values: &Values<'_>, body: &str, mode: ParseMode) -> String {
    const PLACEHOLDERS: [&str; 5] = [
        "{from}",
        "{to}",
        "{subject}",
        "{body}",
        "{attachments_details}",
    ];
    let mut out = String::with_capacity(template.len() + body.len() + 256);
    let mut rest = template;
    'outer: while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("\\n") {
            out.push('\n');
            rest = after;
            continue;
        }
        if rest.starts_with('{') {
            for (index, placeholder) in PLACEHOLDERS.iter().enumerate() {
                if let Some(after) = rest.strip_prefix(placeholder) {
                    let value = match index {
                        0 => values.from,
                        1 => values.to,
                        2 => values.subject,
                        3 => body,
                        _ => values.attachments_details,
                    };
                    out.push_str(&mode.escape(value));
                    rest = after;
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().expect("rest is not empty");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out.trim().to_string()
}

/// Message length the way Telegram counts it (UTF-16 code units).
pub fn telegram_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of `s` that is at most `limit` UTF-16 code units long
/// after escaping for `mode`.
fn prefix_within(s: &str, limit: usize, mode: ParseMode) -> &str {
    let mut used = 0;
    let mut buf = [0u8; 4];
    for (index, c) in s.char_indices() {
        let cost = telegram_len(&mode.escape(c.encode_utf8(&mut buf)));
        if used + cost > limit {
            return &s[..index];
        }
        used += cost;
    }
    s
}

/// The message text and whether it had to be truncated.
struct Text {
    message: String,
    parse_mode: Option<ParseMode>,
    /// The complete text, set only when `message` is truncated.
    full: Option<String>,
}

fn build_text(cfg: &FormatConfig, values: &Values<'_>, body: &str) -> Text {
    let mode = cfg.parse_mode;
    let limit = cfg.message_length_to_send_as_file;
    let body = body.trim();
    let message = render(&cfg.template, values, body, mode);
    if telegram_len(&message) <= limit {
        return Text {
            message,
            parse_mode: mode.api_value().map(|_| mode),
            full: None,
        };
    }

    // The file always carries plain text: values unescaped.
    let full = render(&cfg.template, values, body, ParseMode::None);

    // How long is the message with a one-character body?
    let placeholder_body = format!(".{TRUNCATION_MARKER}");
    let empty = render(&cfg.template, values, &placeholder_body, mode);
    let empty_len = telegram_len(&empty);
    if empty_len >= limit {
        // Even the empty message is too long: cut the plain text hard and
        // send it without a parse mode, since the cut may split markup.
        let cut = prefix_within(&full, limit, ParseMode::None).to_string();
        return Text {
            message: cut,
            parse_mode: None,
            full: Some(full),
        };
    }

    // `empty` already pays for the marker and one character of body, so the
    // body prefix may use what is left (the Go version's arithmetic, which
    // keeps one character in reserve).
    let prefix = prefix_within(body, limit - empty_len, mode);
    let truncated_body = format!("{prefix}{TRUNCATION_MARKER}");
    let message = render(&cfg.template, values, truncated_body.trim(), mode);
    debug_assert!(telegram_len(&message) <= limit);
    Text {
        message,
        parse_mode: mode.api_value().map(|_| mode),
        full: Some(full),
    }
}

fn is_photo_type(content_type: &str) -> bool {
    matches!(content_type, "image/jpeg" | "image/png")
}

/// `application/octet-stream` says nothing; guess from the file name like
/// the Go version did.
fn guess_content_type(content_type: String, filename: Option<&str>) -> String {
    if content_type != "application/octet-stream" {
        return content_type;
    }
    filename
        .and_then(|name| mime_guess::from_path(name).first_raw())
        .map(str::to_string)
        .unwrap_or(content_type)
}

fn default_extension(content_type: &str) -> &'static str {
    match content_type {
        "text/plain" => ".txt",
        "text/html" => ".html",
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "application/pdf" => ".pdf",
        "message/rfc822" => ".eml",
        _ => ".bin",
    }
}

fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Telegram limits captions to 1024 characters.
fn caption(name: &str) -> String {
    name.chars().take(1024).collect()
}

struct Part {
    inline: bool,
    filename: Option<String>,
    content_type: String,
    content: Vec<u8>,
}

struct Parsed {
    subject: String,
    body: String,
    parts: Vec<Part>,
}

fn content_type_of(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub),
            None => ct.ctype().to_string(),
        }
        .to_ascii_lowercase(),
        None => match &part.body {
            PartType::Text(_) => "text/plain".to_string(),
            PartType::Html(_) => "text/html".to_string(),
            PartType::Message(_) => "message/rfc822".to_string(),
            _ => "application/octet-stream".to_string(),
        },
    }
}

fn parse(data: &[u8]) -> Parsed {
    let Some(message) = MessageParser::default().parse(data) else {
        return Parsed {
            subject: String::new(),
            body: String::from_utf8_lossy(data).into_owned(),
            parts: Vec::new(),
        };
    };

    let subject = message.subject().unwrap_or_default().to_string();

    let texts: Vec<String> = (0..message.text_body_count())
        .filter_map(|i| message.body_text(i).map(Cow::into_owned))
        .filter(|t| !t.trim().is_empty())
        .collect();
    let mut body = texts.join("\n");

    let mut body_part = None;
    if body.trim().is_empty() {
        // GNU mailx: the text is a text/plain part with
        // `Content-Disposition: attachment` and no file name.
        body_part = message.attachments.iter().copied().find(|&id| {
            message.part(id).is_some_and(|p| {
                matches!(p.body, PartType::Text(_))
                    && p.attachment_name().is_none()
                    && content_type_of(p) == "text/plain"
            })
        });
        if let Some(id) = body_part {
            body = message
                .part(id)
                .and_then(|p| p.text_contents())
                .unwrap_or_default()
                .to_string();
        }
    }

    let parts = message
        .attachments
        .iter()
        .copied()
        .filter(|&id| Some(id) != body_part)
        .filter_map(|id| message.part(id))
        .filter(|p| !p.is_multipart())
        .map(|p| {
            let filename = p.attachment_name().map(str::to_string);
            let disposition = p.content_disposition();
            let inline = match disposition {
                Some(d) if d.is_attachment() => false,
                Some(d) if d.is_inline() => true,
                _ => matches!(p.body, PartType::InlineBinary(_)) || p.content_id().is_some(),
            };
            let content_type = guess_content_type(content_type_of(p), filename.as_deref());
            Part {
                inline,
                filename,
                content_type,
                content: p.contents().to_vec(),
            }
        })
        .collect();

    Parsed {
        subject,
        body,
        parts,
    }
}

pub fn format_email(envelope: &Envelope<'_>, cfg: &FormatConfig) -> FormattedEmail {
    let parsed = parse(envelope.data);

    let mut details = Vec::new();
    let mut attachments = Vec::new();
    for (index, part) in parsed.parts.into_iter().enumerate() {
        let size = part.content.len() as u64;
        let display_name = part.filename.clone().unwrap_or_else(|| {
            format!(
                "attachment-{}{}",
                index + 1,
                default_extension(&part.content_type)
            )
        });
        let kind = if cfg.attachment_max_photo_size > 0
            && is_photo_type(&part.content_type)
            && size <= cfg.attachment_max_photo_size
        {
            Some(AttachmentKind::Photo)
        } else if cfg.attachment_max_size > 0 && size <= cfg.attachment_max_size {
            Some(AttachmentKind::Document)
        } else {
            None
        };
        details.push(format!(
            "- {} {} ({}) {}, {}",
            if part.inline { "🔗" } else { "📎" },
            display_name,
            part.content_type,
            human_size(size),
            if kind.is_some() {
                "sending..."
            } else {
                "discarded"
            },
        ));
        if let Some(kind) = kind {
            attachments.push(Attachment {
                kind,
                filename: basename(&display_name).to_string(),
                caption: caption(&display_name),
                content_type: part.content_type,
                content: part.content,
            });
        }
    }
    let attachments_details = if details.is_empty() {
        String::new()
    } else {
        format!("Attachments:\n{}", details.join("\n"))
    };

    let to = envelope.rcpt_to.join(", ");
    let values = Values {
        from: envelope.mail_from,
        to: &to,
        subject: &parsed.subject,
        attachments_details: &attachments_details,
    };
    let text = build_text(cfg, &values, &parsed.body);

    let mut full_text_dropped = false;
    if let Some(full) = text.full {
        if cfg.attachment_max_size > 0 && full.len() as u64 <= cfg.attachment_max_size {
            attachments.insert(
                0,
                Attachment {
                    kind: AttachmentKind::Document,
                    filename: FULL_MESSAGE_FILENAME.to_string(),
                    caption: FULL_MESSAGE_CAPTION.to_string(),
                    content_type: "text/plain".to_string(),
                    content: full.into_bytes(),
                },
            );
        } else {
            full_text_dropped = true;
        }
    }

    FormattedEmail {
        text: text.message,
        parse_mode: text.parse_mode,
        attachments,
        full_text_dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FormatConfig {
        FormatConfig::default()
    }

    fn format(data: &str, cfg: &FormatConfig) -> FormattedEmail {
        let rcpt = vec!["to@test".to_string()];
        format_email(
            &Envelope {
                mail_from: "from@test",
                rcpt_to: &rcpt,
                data: data.as_bytes(),
            },
            cfg,
        )
    }

    #[test]
    fn unparseable_data_is_the_body() {
        let out = format("hi\r\n", &cfg());
        assert_eq!(out.text, "From: from@test\nTo: to@test\nSubject: \n\nhi");
        assert_eq!(out.parse_mode, None);
        assert!(out.attachments.is_empty());
    }

    #[test]
    fn decodes_encoded_words_and_quoted_printable() {
        let out = format(
            "Subject: =?UTF-8?B?8J+Yjg==?=\r\nContent-Type: text/plain; charset=UTF-8\r\n\
             Content-Transfer-Encoding: quoted-printable\r\n\r\n=F0=9F=92=A9\r\n",
            &cfg(),
        );
        assert_eq!(out.text, "From: from@test\nTo: to@test\nSubject: 😎\n\n💩");
    }

    #[test]
    fn decodes_latin1() {
        let out = format(
            "Subject: =?ISO-8859-1?Q?Anna-V=E9ronique?=\nTo: to@test\nMIME-Version: 1.0\n\
             Content-Type: text/plain; charset=ISO-8859-1\nContent-Transfer-Encoding: base64\n\n\
             QW5uYS1W6XJvbmlxdWUK\n",
            &cfg(),
        );
        assert_eq!(
            out.text,
            "From: from@test\nTo: to@test\nSubject: Anna-Véronique\n\nAnna-Véronique"
        );
    }

    #[test]
    fn template_is_rendered_in_one_pass() {
        let mut c = cfg();
        c.template = "[{subject}] {body}\\n-- {from}".to_string();
        let out = format("Subject: {body} \\n\r\n\r\ntext with {from}\r\n", &c);
        assert_eq!(out.text, "[{body} \\n] text with {from}\n-- from@test");
    }

    #[test]
    fn markdown_v2_escapes_values_not_the_template() {
        let mut c = cfg();
        c.template = "*{subject}*\\n{body}".to_string();
        c.parse_mode = ParseMode::MarkdownV2;
        let out = format("Subject: disk 95% full!\r\n\r\nsda1 (root) > 95.0%\r\n", &c);
        assert_eq!(out.text, "*disk 95% full\\!*\nsda1 \\(root\\) \\> 95\\.0%");
        assert_eq!(out.parse_mode, Some(ParseMode::MarkdownV2));
    }

    #[test]
    fn html_escapes_values() {
        let mut c = cfg();
        c.template = "<b>{subject}</b>\\n{body}".to_string();
        c.parse_mode = ParseMode::Html;
        let out = format("Subject: a<b>&c\r\n\r\n1 < 2\r\n", &c);
        assert_eq!(out.text, "<b>a&lt;b&gt;&amp;c</b>\n1 &lt; 2");
        assert_eq!(out.parse_mode, Some(ParseMode::Html));
    }

    #[test]
    fn mutt_message() {
        let mut c = cfg();
        c.attachment_max_size = 1024;
        let out = format(
            "Date: Sun, 29 Aug 2021 21:30:10 +0300\nFrom: from@test\nTo: to@test\nSubject: test\n\
             MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"TB36FDmn/VVEgNH/\"\n\
             Content-Disposition: inline\n\n\n--TB36FDmn/VVEgNH/\n\
             Content-Type: text/plain; charset=us-ascii\nContent-Disposition: inline\n\n\
             Sun 29 Aug 2021 09:30:10 PM MSK\n\n--TB36FDmn/VVEgNH/\n\
             Content-Type: text/plain; charset=us-ascii\nContent-Disposition: attachment; filename=tt\n\n\
             hoho\n\n--TB36FDmn/VVEgNH/--\n",
            &c,
        );
        assert_eq!(
            out.text,
            "From: from@test\nTo: to@test\nSubject: test\n\nSun 29 Aug 2021 09:30:10 PM MSK\n\n\
             Attachments:\n- 📎 tt (text/plain) 5B, sending..."
        );
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].filename, "tt");
        assert_eq!(out.attachments[0].content, b"hoho\n");
        assert_eq!(out.attachments[0].kind, AttachmentKind::Document);
    }

    #[test]
    fn mailx_message() {
        let mut c = cfg();
        c.attachment_max_size = 1024;
        let out = format(
            "MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"1493203554-1630261823=:345292\"\n\
             Subject: test\nTo: to@test\nFrom: from@test\n\n\
             --1493203554-1630261823=:345292\nContent-Type: text/plain; charset=UTF-8\n\
             Content-Disposition: attachment\nContent-Transfer-Encoding: 8bit\n\n\
             Sun 29 Aug 2021 09:30:23 PM MSK\n\n\
             --1493203554-1630261823=:345292\nContent-Type: application/octet-stream; name=\"tt\"\n\
             Content-Disposition: attachment; filename=\"./tt\"\nContent-Transfer-Encoding: base64\n\n\
             aG9obwo=\n--1493203554-1630261823=:345292--\n",
            &c,
        );
        assert_eq!(
            out.text,
            "From: from@test\nTo: to@test\nSubject: test\n\nSun 29 Aug 2021 09:30:23 PM MSK\n\n\
             Attachments:\n- 📎 ./tt (application/octet-stream) 5B, sending..."
        );
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].filename, "tt");
        assert_eq!(out.attachments[0].caption, "./tt");
        assert_eq!(out.attachments[0].content, b"hoho\n");
    }

    #[test]
    fn long_message_is_truncated_with_the_full_text_attached() {
        let mut c = cfg();
        c.message_length_to_send_as_file = 100;
        let body = "Hello_".repeat(60);
        let out = format(&format!("Subject: Test subj\r\n\r\n{body}\r\n"), &c);
        assert_eq!(
            out.text,
            "From: from@test\nTo: to@test\nSubject: Test subj\n\nHello_Hello_Hello_Hello_Hello_Hello_He\n\n[truncated]"
        );
        assert!(telegram_len(&out.text) <= 100);
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].filename, FULL_MESSAGE_FILENAME);
        assert_eq!(
            String::from_utf8(out.attachments[0].content.clone()).unwrap(),
            format!("From: from@test\nTo: to@test\nSubject: Test subj\n\n{body}")
        );
    }

    #[test]
    fn hopelessly_long_template_is_cut_hard_as_plain_text() {
        let mut c = cfg();
        c.message_length_to_send_as_file = 12;
        c.parse_mode = ParseMode::MarkdownV2;
        let out = format("Subject: Test subj\r\n\r\nHello_Hello_Hello_\r\n", &c);
        assert_eq!(out.text, "From: from@t");
        assert_eq!(out.parse_mode, None);
        assert_eq!(out.attachments[0].filename, FULL_MESSAGE_FILENAME);
    }

    #[test]
    fn truncation_counts_escaped_length() {
        let mut c = cfg();
        c.template = "{body}".to_string();
        c.parse_mode = ParseMode::MarkdownV2;
        c.message_length_to_send_as_file = 40;
        // Every '.' costs two code units once escaped; the empty message
        // ("\." plus the escaped marker) is 17, leaving 23 for the body.
        let out = format(&format!("Subject: s\r\n\r\n{}\r\n", ".".repeat(100)), &c);
        assert_eq!(out.text, format!("{}\n\n\\[truncated\\]", "\\.".repeat(11)));
        assert!(telegram_len(&out.text) <= 40);
        assert_eq!(out.parse_mode, Some(ParseMode::MarkdownV2));
        assert_eq!(
            out.attachments[0].content,
            ".".repeat(100).into_bytes(),
            "the attached full text is not escaped"
        );
    }

    #[test]
    fn emoji_count_as_two_code_units() {
        assert_eq!(telegram_len("📎"), 2);
        assert_eq!(telegram_len("é"), 1);
        assert_eq!(prefix_within("a📎b", 2, ParseMode::None), "a");
        assert_eq!(prefix_within("a📎b", 3, ParseMode::None), "a📎");
    }

    #[test]
    fn full_text_too_large_for_a_document_is_dropped_not_fatal() {
        let mut c = cfg();
        c.message_length_to_send_as_file = 50;
        c.attachment_max_size = 0;
        let out = format(&format!("Subject: s\r\n\r\n{}\r\n", "x".repeat(500)), &c);
        assert!(telegram_len(&out.text) <= 50);
        assert!(out.attachments.is_empty());
        assert!(out.full_text_dropped);
    }
}
