//! `smtp_to_telegram`: a small SMTP server that forwards every incoming email
//! to one or more Telegram chats.
//!
//! The binary (`src/main.rs`) only parses the command line and wires up
//! signals; everything else lives here so the integration tests can run the
//! real server in-process.

pub mod auth;
pub mod config;
pub mod delivery;
pub mod format;
pub mod size;
pub mod smtp;
pub mod telegram;

pub use config::{Config, FormatConfig, ParseMode, SmtpConfig, TelegramConfig};
pub use smtp::serve;
