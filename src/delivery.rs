//! Delivers one received email to every configured chat.
//!
//! Delivery is synchronous with the SMTP transaction, as in the Go version:
//! the client gets `250` only after Telegram accepted the message for every
//! chat, and a temporary `451` otherwise, so the sender keeps the mail and
//! retries. There is no queue here.

use tracing::warn;

use crate::config::{FormatConfig, TelegramConfig};
use crate::format::{Envelope, format_email};
use crate::telegram::{TelegramClient, TelegramError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub chats: usize,
    pub attachments_sent: usize,
    pub attachments_failed: usize,
}

pub async fn deliver(
    envelope: &Envelope<'_>,
    format: &FormatConfig,
    telegram: &TelegramConfig,
    client: &TelegramClient,
) -> Result<Report, TelegramError> {
    let email = format_email(envelope, format);
    if email.full_text_dropped {
        warn!(
            "message truncated; the full text is larger than \
             --forwarded-attachment-max-size and is not attached"
        );
    }

    let mut report = Report {
        chats: 0,
        attachments_sent: 0,
        attachments_failed: 0,
    };
    for chat_id in &telegram.chat_ids {
        // Failing to deliver the text to any chat fails the whole email.
        let message_id = client
            .send_message(chat_id, &email.text, email.parse_mode)
            .await?;
        report.chats += 1;

        for attachment in &email.attachments {
            match client
                .send_attachment(chat_id, attachment, message_id)
                .await
            {
                Ok(()) => report.attachments_sent += 1,
                Err(error) if telegram.attachment_respect_errors => return Err(error),
                Err(error) => {
                    report.attachments_failed += 1;
                    warn!(chat_id = %chat_id, file = %attachment.filename, "ignoring attachment error: {error}");
                }
            }
        }
    }
    Ok(report)
}
