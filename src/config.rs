//! Command line / environment configuration.
//!
//! Every option is a command line flag with an `ST_*` environment variable
//! as fallback. Secrets are the exception: the bot token and the SMTP
//! credentials can only come from a file (`--*-file`, meant for systemd
//! `LoadCredential=`) or from an environment variable, never from argv, which
//! any local user can read through `ps`.

use std::borrow::Cow;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use ipnet::IpNet;

use crate::auth::Credentials;
use crate::size::parse_size;

pub const DEFAULT_TEMPLATE: &str =
    "From: {from}\\nTo: {to}\\nSubject: {subject}\\n\\n{body}\\n\\n{attachments_details}";

/// Environment variable holding the bot token (there is no flag for it).
pub const ENV_BOT_TOKEN: &str = "ST_TELEGRAM_BOT_TOKEN";
/// Environment variable holding SMTP credentials (there is no flag for it).
pub const ENV_CREDENTIALS: &str = "ST_SMTP_CREDENTIALS";
/// Removed Go-era option; refusing to start beats silently ignoring it.
const ENV_REMOVED_API_POSTFIX: &str = "ST_TELEGRAM_API_POSTFIX";

/// Form fields this program sets itself; `--telegram-api-extra-param` may not
/// override them.
const RESERVED_PARAMS: &[&str] = &[
    "chat_id",
    "text",
    "parse_mode",
    "caption",
    "document",
    "photo",
    "reply_parameters",
    "reply_to_message_id",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ParseMode {
    /// Plain text, nothing is escaped.
    #[default]
    #[value(name = "none")]
    None,
    /// Telegram MarkdownV2; substituted values are escaped.
    #[value(name = "MarkdownV2")]
    MarkdownV2,
    /// Telegram HTML; substituted values are escaped.
    #[value(name = "HTML")]
    Html,
    /// Telegram legacy Markdown; substituted values are escaped.
    #[value(name = "Markdown")]
    Markdown,
}

impl ParseMode {
    /// The value of the Bot API `parse_mode` parameter, if any.
    pub fn api_value(self) -> Option<&'static str> {
        match self {
            ParseMode::None => None,
            ParseMode::MarkdownV2 => Some("MarkdownV2"),
            ParseMode::Html => Some("HTML"),
            ParseMode::Markdown => Some("Markdown"),
        }
    }

    /// Escapes text so Telegram shows it literally under this parse mode.
    pub fn escape(self, s: &str) -> Cow<'_, str> {
        let needs: fn(char) -> bool = match self {
            ParseMode::None => return Cow::Borrowed(s),
            ParseMode::MarkdownV2 => |c| "_*[]()~`>#+-=|{}.!\\".contains(c),
            ParseMode::Markdown => |c| "_*`[".contains(c),
            ParseMode::Html => |c| matches!(c, '&' | '<' | '>'),
        };
        if !s.chars().any(needs) {
            return Cow::Borrowed(s);
        }
        let mut out = String::with_capacity(s.len() + s.len() / 8);
        for c in s.chars() {
            if !needs(c) {
                out.push(c);
            } else if self == ParseMode::Html {
                out.push_str(match c {
                    '&' => "&amp;",
                    '<' => "&lt;",
                    _ => "&gt;",
                });
            } else {
                out.push('\\');
                out.push(c);
            }
        }
        Cow::Owned(out)
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "smtp_to_telegram",
    version,
    about = "A small SMTP server that forwards every incoming email to Telegram chats.",
    after_help = "Secrets are never taken from the command line:\n  \
        bot token:         --telegram-bot-token-file, or the ST_TELEGRAM_BOT_TOKEN environment variable\n  \
        SMTP credentials:  --credentials-file, or the ST_SMTP_CREDENTIALS environment variable\n                     \
        (one `username:password` per line)"
)]
pub struct Cli {
    /// SMTP: address to listen on; repeat the flag (or separate with commas) for several
    #[arg(
        long,
        env = "ST_SMTP_LISTEN",
        value_delimiter = ',',
        default_value = "127.0.0.1:2525"
    )]
    pub smtp_listen: Vec<SocketAddr>,

    /// SMTP: host name used in the greeting and the EHLO reply [default: the system host name]
    #[arg(long, env = "ST_SMTP_PRIMARY_HOST")]
    pub smtp_primary_host: Option<String>,

    /// SMTP: largest accepted message, e.g. 10m, 50MB, 4MiB
    #[arg(long, env = "ST_SMTP_MAX_ENVELOPE_SIZE", default_value = "50m", value_parser = parse_size)]
    pub smtp_max_envelope_size: u64,

    /// SMTP: maximum number of simultaneous connections
    #[arg(long, env = "ST_SMTP_MAX_CONNECTIONS", default_value_t = 100)]
    pub smtp_max_connections: usize,

    /// SMTP: seconds to wait for each command or line of data before closing the connection
    #[arg(long, env = "ST_SMTP_TIMEOUT_SECONDS", default_value_t = 30)]
    pub smtp_timeout_seconds: u64,

    /// SMTP: accept mail from clients that did not authenticate
    #[arg(long, env = "ST_SMTP_ALLOW_ANONYMOUS")]
    pub allow_anonymous: bool,

    /// SMTP: file with `username:password` lines accepted by AUTH PLAIN/LOGIN
    #[arg(long, env = "ST_SMTP_CREDENTIALS_FILE")]
    pub credentials_file: Option<PathBuf>,

    /// SMTP: client networks to which AUTH is offered over the unencrypted connection
    #[arg(
        long,
        env = "ST_SMTP_PLAINTEXT_AUTH_NETWORKS",
        value_delimiter = ',',
        default_value = "127.0.0.0/8,::1/128"
    )]
    pub plaintext_auth_networks: Vec<IpNet>,

    /// Telegram: file containing the bot token
    #[arg(long, env = "ST_TELEGRAM_BOT_TOKEN_FILE")]
    pub telegram_bot_token_file: Option<PathBuf>,

    /// Telegram: chat ids to deliver to, comma-separated
    #[arg(long, env = "ST_TELEGRAM_CHAT_IDS", value_delimiter = ',')]
    pub telegram_chat_ids: Vec<String>,

    /// Telegram: file with chat ids, separated by commas or whitespace
    #[arg(long, env = "ST_TELEGRAM_CHAT_IDS_FILE")]
    pub telegram_chat_ids_file: Option<PathBuf>,

    /// Telegram: Bot API URL prefix; requests go to {prefix}bot{token}/{method}
    #[arg(
        long,
        env = "ST_TELEGRAM_API_PREFIX",
        default_value = "https://api.telegram.org/"
    )]
    pub telegram_api_prefix: String,

    /// Telegram: timeout of each Bot API request, in seconds
    #[arg(long, env = "ST_TELEGRAM_API_TIMEOUT_SECONDS", default_value_t = 30.0)]
    pub telegram_api_timeout_seconds: f64,

    /// Telegram: parse_mode of the message; values substituted into the template are escaped for it
    #[arg(
        long,
        env = "ST_TELEGRAM_API_PARSE_MODE",
        value_enum,
        ignore_case = true,
        default_value = "none"
    )]
    pub telegram_api_parse_mode: ParseMode,

    /// Telegram: extra KEY=VALUE parameter for every send request (e.g. message_thread_id=42); repeatable
    #[arg(long, env = "ST_TELEGRAM_API_EXTRA_PARAMS", value_delimiter = ',', value_parser = parse_key_value)]
    pub telegram_api_extra_param: Vec<(String, String)>,

    /// Message template; placeholders {from} {to} {subject} {body} {attachments_details}, and `\n` for a newline
    #[arg(
        long,
        env = "ST_TELEGRAM_MESSAGE_TEMPLATE",
        conflicts_with = "message_template_file"
    )]
    pub message_template: Option<String>,

    /// File containing the message template
    #[arg(long, env = "ST_TELEGRAM_MESSAGE_TEMPLATE_FILE")]
    pub message_template_file: Option<PathBuf>,

    /// Messages longer than this (in UTF-16 code units, as Telegram counts) are truncated and the full text is attached as full_message.txt
    #[arg(
        long,
        env = "ST_MESSAGE_LENGTH_TO_SEND_AS_FILE",
        default_value_t = 4095
    )]
    pub message_length_to_send_as_file: usize,

    /// Largest attachment forwarded as a document; 0 disables forwarding (Telegram's own limit is 50MB)
    #[arg(long, env = "ST_FORWARDED_ATTACHMENT_MAX_SIZE", default_value = "10m", value_parser = parse_size)]
    pub forwarded_attachment_max_size: u64,

    /// Largest JPEG/PNG attachment forwarded as a photo; 0 disables (Telegram's own limit is 10MB)
    #[arg(long, env = "ST_FORWARDED_ATTACHMENT_MAX_PHOTO_SIZE", default_value = "10m", value_parser = parse_size)]
    pub forwarded_attachment_max_photo_size: u64,

    /// Reject the whole email (with a temporary error) when an attachment cannot be forwarded
    #[arg(long, env = "ST_FORWARDED_ATTACHMENT_RESPECT_ERRORS")]
    pub forwarded_attachment_respect_errors: bool,

    /// Log level: error, warn, info, debug or trace
    #[arg(long, env = "ST_LOG_LEVEL", default_value = "info")]
    pub log_level: tracing::Level,
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("empty key in {s:?}"));
    }
    Ok((key.to_string(), value.to_string()))
}

/// Fully resolved configuration: files read, secrets loaded, values checked.
#[derive(Clone, Debug)]
pub struct Config {
    pub smtp: SmtpConfig,
    pub telegram: TelegramConfig,
    pub format: FormatConfig,
}

#[derive(Clone, Debug)]
pub struct SmtpConfig {
    pub listen: Vec<SocketAddr>,
    pub hostname: String,
    pub max_message_size: u64,
    pub max_connections: usize,
    pub timeout: Duration,
    pub allow_anonymous: bool,
    pub credentials: Credentials,
    pub plaintext_auth_networks: Vec<IpNet>,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            listen: vec![SocketAddr::from(([127, 0, 0, 1], 2525))],
            hostname: "localhost".to_string(),
            max_message_size: 50_000_000,
            max_connections: 100,
            timeout: Duration::from_secs(30),
            allow_anonymous: false,
            credentials: Credentials::default(),
            plaintext_auth_networks: vec![
                "127.0.0.0/8".parse().expect("valid network"),
                "::1/128".parse().expect("valid network"),
            ],
        }
    }
}

#[derive(Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_ids: Vec<String>,
    pub api_prefix: String,
    pub timeout: Duration,
    pub extra_params: Vec<(String, String)>,
    pub attachment_respect_errors: bool,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"***")
            .field("chat_ids", &self.chat_ids)
            .field("api_prefix", &self.api_prefix)
            .field("timeout", &self.timeout)
            .field("extra_params", &self.extra_params)
            .field("attachment_respect_errors", &self.attachment_respect_errors)
            .finish()
    }
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            chat_ids: Vec::new(),
            api_prefix: "https://api.telegram.org/".to_string(),
            timeout: Duration::from_secs(30),
            extra_params: Vec::new(),
            attachment_respect_errors: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FormatConfig {
    pub template: String,
    pub parse_mode: ParseMode,
    pub message_length_to_send_as_file: usize,
    pub attachment_max_size: u64,
    pub attachment_max_photo_size: u64,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            template: DEFAULT_TEMPLATE.to_string(),
            parse_mode: ParseMode::None,
            message_length_to_send_as_file: 4095,
            attachment_max_size: 10_000_000,
            attachment_max_photo_size: 10_000_000,
        }
    }
}

fn read_file(path: &Path, what: &str) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("cannot read {what} from {}", path.display()))
}

impl Config {
    /// Resolves the parsed command line. `env` looks up the secret-only
    /// environment variables (it is `std::env::var` outside of tests).
    pub fn from_cli(cli: Cli, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        if env(ENV_REMOVED_API_POSTFIX).is_some() {
            bail!(
                "{ENV_REMOVED_API_POSTFIX} is no longer supported: use --telegram-api-parse-mode \
                 (ST_TELEGRAM_API_PARSE_MODE) and --telegram-api-extra-param (ST_TELEGRAM_API_EXTRA_PARAMS)"
            );
        }

        let bot_token = match (&cli.telegram_bot_token_file, env(ENV_BOT_TOKEN)) {
            (Some(_), Some(_)) => bail!(
                "the bot token is given both by --telegram-bot-token-file and {ENV_BOT_TOKEN}; use one"
            ),
            (Some(path), None) => read_file(path, "the bot token")?.trim().to_string(),
            (None, Some(token)) => token.trim().to_string(),
            (None, None) => bail!(
                "the Telegram bot token is missing: use --telegram-bot-token-file or {ENV_BOT_TOKEN}"
            ),
        };
        if bot_token.is_empty() {
            bail!("the Telegram bot token is empty");
        }
        if bot_token.chars().any(|c| c.is_whitespace() || c == '/') {
            bail!("the Telegram bot token contains whitespace or '/'");
        }

        let chat_ids: Vec<String> = match (
            &cli.telegram_chat_ids_file,
            cli.telegram_chat_ids.is_empty(),
        ) {
            (Some(_), false) => bail!(
                "chat ids are given both by --telegram-chat-ids and --telegram-chat-ids-file; use one"
            ),
            (Some(path), true) => read_file(path, "the chat ids")?
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(str::to_string)
                .collect(),
            (None, _) => cli.telegram_chat_ids.clone(),
        };
        let chat_ids: Vec<String> = chat_ids
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if chat_ids.is_empty() {
            bail!(
                "Telegram chat ids are missing: use --telegram-chat-ids or --telegram-chat-ids-file"
            );
        }

        let credentials = match (&cli.credentials_file, env(ENV_CREDENTIALS)) {
            (Some(_), Some(_)) => bail!(
                "SMTP credentials are given both by --credentials-file and {ENV_CREDENTIALS}; use one"
            ),
            (Some(path), None) => Credentials::parse(&read_file(path, "SMTP credentials")?)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
            (None, Some(text)) => {
                Credentials::parse(&text).map_err(|e| anyhow::anyhow!("{ENV_CREDENTIALS}: {e}"))?
            }
            (None, None) => Credentials::default(),
        };
        if credentials.is_empty() && !cli.allow_anonymous {
            bail!(
                "nobody could deliver mail: no SMTP credentials are configured and \
                 --allow-anonymous is off"
            );
        }

        let template = match (&cli.message_template, &cli.message_template_file) {
            (Some(t), _) => t.clone(),
            (None, Some(path)) => read_file(path, "the message template")?,
            (None, None) => DEFAULT_TEMPLATE.to_string(),
        };

        if !(1..=4096).contains(&cli.message_length_to_send_as_file) {
            bail!("--message-length-to-send-as-file must be between 1 and 4096");
        }
        if !(cli.telegram_api_timeout_seconds.is_finite() && cli.telegram_api_timeout_seconds > 0.0)
        {
            bail!("--telegram-api-timeout-seconds must be a positive number");
        }
        if cli.smtp_timeout_seconds == 0 {
            bail!("--smtp-timeout-seconds must be positive");
        }
        if cli.smtp_max_connections == 0 {
            bail!("--smtp-max-connections must be positive");
        }
        if cli.smtp_listen.is_empty() {
            bail!("--smtp-listen needs at least one address");
        }
        for (key, _) in &cli.telegram_api_extra_param {
            if RESERVED_PARAMS.contains(&key.as_str()) {
                bail!("--telegram-api-extra-param cannot set `{key}`: it is set by the program");
            }
        }

        let hostname = match cli.smtp_primary_host {
            Some(h) => h,
            None => gethostname::gethostname()
                .into_string()
                .map_err(|_| anyhow::anyhow!("the system host name is not valid UTF-8"))?,
        };
        if hostname.is_empty() || hostname.contains(|c: char| c.is_whitespace() || c.is_control()) {
            bail!("invalid SMTP host name {hostname:?}");
        }

        Ok(Config {
            smtp: SmtpConfig {
                listen: cli.smtp_listen,
                hostname,
                max_message_size: cli.smtp_max_envelope_size,
                max_connections: cli.smtp_max_connections,
                timeout: Duration::from_secs(cli.smtp_timeout_seconds),
                allow_anonymous: cli.allow_anonymous,
                credentials,
                plaintext_auth_networks: cli.plaintext_auth_networks,
            },
            telegram: TelegramConfig {
                bot_token,
                chat_ids,
                api_prefix: cli.telegram_api_prefix,
                timeout: Duration::from_secs_f64(cli.telegram_api_timeout_seconds),
                extra_params: cli.telegram_api_extra_param,
                attachment_respect_errors: cli.forwarded_attachment_respect_errors,
            },
            format: FormatConfig {
                template,
                parse_mode: cli.telegram_api_parse_mode,
                message_length_to_send_as_file: cli.message_length_to_send_as_file,
                attachment_max_size: cli.forwarded_attachment_max_size,
                attachment_max_photo_size: cli.forwarded_attachment_max_photo_size,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["smtp_to_telegram"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("valid command line")
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn escapes_markdown_v2() {
        assert_eq!(
            ParseMode::MarkdownV2.escape("a_b*c[d](e)~`>#+-=|{}.!\\"),
            "a\\_b\\*c\\[d\\]\\(e\\)\\~\\`\\>\\#\\+\\-\\=\\|\\{\\}\\.\\!\\\\"
        );
        assert_eq!(
            ParseMode::Html.escape("<b>&</b>"),
            "&lt;b&gt;&amp;&lt;/b&gt;"
        );
        assert_eq!(ParseMode::Markdown.escape("_*`[]"), "\\_\\*\\`\\[]");
        assert_eq!(ParseMode::None.escape("<_>"), "<_>");
    }

    #[test]
    fn minimal_config_with_env_secrets() {
        let config = Config::from_cli(
            cli(&[
                "--telegram-chat-ids",
                "42,-100123",
                "--smtp-primary-host",
                "relay",
            ]),
            env(&[(ENV_BOT_TOKEN, "123:abc\n"), (ENV_CREDENTIALS, "u:p")]),
        )
        .unwrap();
        assert_eq!(config.telegram.bot_token, "123:abc");
        assert_eq!(config.telegram.chat_ids, vec!["42", "-100123"]);
        assert!(config.smtp.credentials.verify("u", "p"));
        assert!(!config.smtp.allow_anonymous);
        assert_eq!(config.smtp.hostname, "relay");
        assert_eq!(config.smtp.max_message_size, 50_000_000);
        assert_eq!(config.format.template, DEFAULT_TEMPLATE);
        assert_eq!(config.format.parse_mode, ParseMode::None);
    }

    #[test]
    fn refuses_a_config_nobody_can_use() {
        let err = Config::from_cli(
            cli(&["--telegram-chat-ids", "42"]),
            env(&[(ENV_BOT_TOKEN, "123:abc")]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--allow-anonymous"));
        assert!(
            Config::from_cli(
                cli(&["--telegram-chat-ids", "42", "--allow-anonymous"]),
                env(&[(ENV_BOT_TOKEN, "123:abc")]),
            )
            .is_ok()
        );
    }

    #[test]
    fn requires_token_and_chat_ids() {
        assert!(
            Config::from_cli(
                cli(&["--telegram-chat-ids", "42", "--allow-anonymous"]),
                env(&[])
            )
            .is_err()
        );
        assert!(
            Config::from_cli(cli(&["--allow-anonymous"]), env(&[(ENV_BOT_TOKEN, "1:a")])).is_err()
        );
    }

    #[test]
    fn rejects_removed_postfix_option() {
        let err = Config::from_cli(
            cli(&["--telegram-chat-ids", "42", "--allow-anonymous"]),
            env(&[
                (ENV_BOT_TOKEN, "1:a"),
                ("ST_TELEGRAM_API_POSTFIX", "&parse_mode=html"),
            ]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no longer supported"));
    }

    #[test]
    fn parse_mode_is_case_insensitive() {
        let c = cli(&["--telegram-api-parse-mode", "html"]);
        assert_eq!(c.telegram_api_parse_mode, ParseMode::Html);
        let c = cli(&["--telegram-api-parse-mode", "markdownv2"]);
        assert_eq!(c.telegram_api_parse_mode, ParseMode::MarkdownV2);
    }

    #[test]
    fn extra_params_cannot_override_reserved_fields() {
        let err = Config::from_cli(
            cli(&[
                "--telegram-chat-ids",
                "42",
                "--allow-anonymous",
                "--telegram-api-extra-param",
                "chat_id=1",
            ]),
            env(&[(ENV_BOT_TOKEN, "1:a")]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("chat_id"));
        let ok = Config::from_cli(
            cli(&[
                "--telegram-chat-ids",
                "42",
                "--allow-anonymous",
                "--telegram-api-extra-param",
                "message_thread_id=7",
                "--telegram-api-extra-param",
                "disable_notification=true",
            ]),
            env(&[(ENV_BOT_TOKEN, "1:a")]),
        )
        .unwrap();
        assert_eq!(
            ok.telegram.extra_params,
            vec![
                ("message_thread_id".to_string(), "7".to_string()),
                ("disable_notification".to_string(), "true".to_string())
            ]
        );
    }

    #[test]
    fn debug_output_hides_the_token() {
        let config = Config::from_cli(
            cli(&["--telegram-chat-ids", "42", "--allow-anonymous"]),
            env(&[(ENV_BOT_TOKEN, "123:verysecret")]),
        )
        .unwrap();
        assert!(!format!("{config:?}").contains("verysecret"));
    }
}
