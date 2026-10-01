//! A minimal Telegram Bot API client: `sendMessage`, `sendDocument` and
//! `sendPhoto`, over HTTPS with rustls.

use std::fmt;
use std::sync::Arc;

use reqwest::multipart::{Form, Part};
use rustls::RootCertStore;
use serde::Deserialize;
use tracing::{debug, info};

use crate::config::{ParseMode, TelegramConfig};
use crate::format::{Attachment, AttachmentKind};

/// A failed Bot API call. The message is safe to log and to show to the
/// SMTP client: the bot token is masked and it is a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramError(String);

impl fmt::Display for TelegramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TelegramError {}

#[derive(Deserialize)]
struct ApiResponse {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
}

pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    extra_params: Vec<(String, String)>,
}

/// Trust anchors for HTTPS: the system's CA certificates (honouring
/// `SSL_CERT_FILE` / `SSL_CERT_DIR`), or a bundled copy of Mozilla's roots
/// when the system has none, as in a build sandbox or a minimal container.
fn root_store() -> (RootCertStore, &'static str) {
    let native = rustls_native_certs::load_native_certs();
    for error in &native.errors {
        debug!("loading system CA certificates: {error}");
    }
    let mut store = RootCertStore::empty();
    let (added, _) = store.add_parsable_certificates(native.certs);
    if added > 0 {
        return (store, "the system store");
    }
    let mut store = RootCertStore::empty();
    store.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    (
        store,
        "the bundled Mozilla roots (no system CA certificates found)",
    )
}

fn tls_config() -> anyhow::Result<rustls::ClientConfig> {
    let (roots, source) = root_store();
    info!(
        "HTTPS: trusting {} CA certificates from {source}",
        roots.len()
    );
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

impl TelegramClient {
    pub fn new(config: &TelegramConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls_config()?)
            .timeout(config.timeout)
            .user_agent(concat!("smtp_to_telegram/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url: format!("{}bot{}/", config.api_prefix, config.bot_token),
            token: config.bot_token.clone(),
            extra_params: config.extra_params.clone(),
        })
    }

    /// Makes `s` safe for logs and SMTP replies: masks the token (reqwest
    /// errors quote the request URL) and folds it onto one line.
    fn sanitize(&self, s: &str) -> String {
        let masked = if self.token.is_empty() {
            s.to_string()
        } else {
            s.replace(&self.token, "***")
        };
        let mut line = masked.replace('\r', "\\r").replace('\n', "\\n");
        if line.len() > 400 {
            let mut cut = 400;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push_str("...");
        }
        line
    }

    fn error(&self, method: &str, detail: impl fmt::Display) -> TelegramError {
        TelegramError(self.sanitize(&format!("Telegram {method} failed: {detail}")))
    }

    /// Our fields first, then the configured extras; an extra replaces a
    /// field of the same name.
    fn fields(&self, own: Vec<(&'static str, String)>) -> Vec<(String, String)> {
        let mut fields: Vec<(String, String)> =
            own.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        for (key, value) in &self.extra_params {
            match fields.iter_mut().find(|(k, _)| k == key) {
                Some(field) => field.1 = value.clone(),
                None => fields.push((key.clone(), value.clone())),
            }
        }
        fields
    }

    async fn call(
        &self,
        method: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<Option<serde_json::Value>, TelegramError> {
        let response = request
            .send()
            .await
            .map_err(|e| self.error(method, error_chain(&e.without_url())))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| self.error(method, error_chain(&e.without_url())))?;
        let parsed: Option<ApiResponse> = serde_json::from_slice(&body).ok();
        if !status.is_success() {
            let detail = match parsed.and_then(|p| p.description) {
                Some(description) => description,
                None => String::from_utf8_lossy(&body).into_owned(),
            };
            return Err(self.error(method, format!("HTTP {}: {detail}", status.as_u16())));
        }
        match parsed {
            Some(ApiResponse {
                ok: true, result, ..
            }) => Ok(result),
            Some(ApiResponse { description, .. }) => Err(self.error(
                method,
                format!("ok=false: {}", description.unwrap_or_default()),
            )),
            None => Err(self.error(
                method,
                format!("unparseable response: {}", String::from_utf8_lossy(&body)),
            )),
        }
    }

    /// Sends a text message and returns its `message_id`.
    pub async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<i64, TelegramError> {
        let mut own = vec![
            ("chat_id", chat_id.to_string()),
            ("text", text.to_string()),
            (
                "link_preview_options",
                r#"{"is_disabled":true}"#.to_string(),
            ),
        ];
        if let Some(mode) = parse_mode.and_then(ParseMode::api_value) {
            own.push(("parse_mode", mode.to_string()));
        }
        let fields = self.fields(own);
        let request = self
            .http
            .post(format!("{}sendMessage", self.base_url))
            .form(&fields);
        let result = self.call("sendMessage", request).await?;
        result
            .as_ref()
            .and_then(|r| r.get("message_id"))
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| self.error("sendMessage", "response has no result.message_id"))
    }

    /// Uploads an attachment as a reply to `reply_to` without a
    /// notification sound.
    pub async fn send_attachment(
        &self,
        chat_id: &str,
        attachment: &Attachment,
        reply_to: i64,
    ) -> Result<(), TelegramError> {
        let (method, field) = match attachment.kind {
            AttachmentKind::Photo => ("sendPhoto", "photo"),
            AttachmentKind::Document => ("sendDocument", "document"),
        };
        let own = vec![
            ("chat_id", chat_id.to_string()),
            ("caption", attachment.caption.clone()),
            ("disable_notification", "true".to_string()),
            (
                "reply_parameters",
                serde_json::json!({
                    "message_id": reply_to,
                    "allow_sending_without_reply": true,
                })
                .to_string(),
            ),
        ];
        let mut form = Form::new();
        for (key, value) in self.fields(own) {
            form = form.text(key, value);
        }
        let file = Part::bytes(attachment.content.clone())
            .file_name(attachment.filename.clone())
            .mime_str(&attachment.content_type)
            .unwrap_or_else(|_| {
                Part::bytes(attachment.content.clone()).file_name(attachment.filename.clone())
            });
        form = form.part(field, file);
        let request = self
            .http
            .post(format!("{}{method}", self.base_url))
            .multipart(form);
        self.call(method, request).await.map(|_| ())
    }
}

/// reqwest's top-level error says only "error sending request"; the cause
/// (connection refused, timeout, TLS) is further down the chain.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !message.contains(&cause_text) {
            message.push_str(": ");
            message.push_str(&cause_text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(extra: Vec<(&str, &str)>) -> TelegramClient {
        TelegramClient::new(&TelegramConfig {
            bot_token: "123:SECRET".into(),
            chat_ids: vec!["1".into()],
            extra_params: extra
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..TelegramConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn sanitizes_token_and_newlines() {
        let c = client(vec![]);
        let s = c.sanitize("GET https://x/bot123:SECRET/sendMessage\r\nfailed");
        assert_eq!(s, "GET https://x/bot***/sendMessage\\r\\nfailed");
        assert!(c.sanitize(&"é".repeat(500)).ends_with("..."));
    }

    #[test]
    fn extra_params_are_appended_or_override() {
        let c = client(vec![
            ("message_thread_id", "7"),
            ("disable_notification", "false"),
        ]);
        let fields = c.fields(vec![
            ("chat_id", "1".to_string()),
            ("disable_notification", "true".to_string()),
        ]);
        assert_eq!(
            fields,
            vec![
                ("chat_id".to_string(), "1".to_string()),
                ("disable_notification".to_string(), "false".to_string()),
                ("message_thread_id".to_string(), "7".to_string()),
            ]
        );
    }
}
