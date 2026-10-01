//! The SMTP server: a small hand-written state machine (RFC 5321) with
//! AUTH PLAIN/LOGIN (RFC 4954), PIPELINING, SIZE, 8BITMIME and
//! ENHANCEDSTATUSCODES. There is no TLS.
//!
//! Authentication rules:
//! - AUTH is advertised and accepted only when credentials are configured
//!   and the client address is inside `plaintext_auth_networks`. Elsewhere
//!   EHLO does not list AUTH and an AUTH command gets `538 5.7.11`, because
//!   the password would cross an untrusted network in the clear.
//! - Without `allow_anonymous`, `MAIL FROM` before a successful AUTH gets
//!   `530 5.7.0`. With it, both authenticated and anonymous clients may send.
//! - Three failed AUTH attempts close the connection.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, error, info, warn};

use crate::auth::{decode_plain, ip_in_networks};
use crate::config::Config;
use crate::delivery;
use crate::format::Envelope;
use crate::telegram::TelegramClient;

/// Longest accepted command line, CRLF included. RFC 5321 asks for 512;
/// RFC 4954 allows AUTH lines up to 12288, far more than credentials need.
pub const MAX_COMMAND_LINE: usize = 4096;
pub const MAX_RECIPIENTS: usize = 100;
const MAX_UNRECOGNIZED_COMMANDS: u32 = 5;
const MAX_AUTH_FAILURES: u32 = 3;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(60);

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

struct Shared {
    config: Arc<Config>,
    telegram: TelegramClient,
}

/// Opens a listening socket. An IPv6 address is bound IPv6-only, so
/// `0.0.0.0:25` and `[::]:25` can be listed together instead of colliding
/// on Linux's dual-stack default. Must be called inside a tokio runtime.
pub fn bind_listener(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Serves SMTP on `listeners` until `shutdown` resolves, then stops
/// accepting, lets sessions finish their current command (a delivery in
/// progress completes) and returns, waiting at most 60 seconds.
pub async fn serve(
    config: Arc<Config>,
    listeners: Vec<TcpListener>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let telegram = TelegramClient::new(&config.telegram)?;
    let max_connections = config.smtp.max_connections.min(u32::MAX as usize);
    let shared = Arc::new(Shared { config, telegram });
    let slots = Arc::new(Semaphore::new(max_connections));
    let (stop_tx, stop_rx) = watch::channel(false);

    let accept_tasks: Vec<_> = listeners
        .into_iter()
        .map(|listener| {
            tokio::spawn(accept_loop(
                listener,
                shared.clone(),
                slots.clone(),
                stop_rx.clone(),
            ))
        })
        .collect();

    shutdown.await;
    info!("shutting down: no longer accepting connections");
    let _ = stop_tx.send(true);
    for task in accept_tasks {
        let _ = task.await;
    }
    match tokio::time::timeout(SHUTDOWN_GRACE, slots.acquire_many(max_connections as u32)).await {
        Ok(_) => info!("all sessions finished"),
        Err(_) => warn!(
            "sessions still open after {}s; exiting anyway",
            SHUTDOWN_GRACE.as_secs()
        ),
    }
    Ok(())
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            _ = stop.wait_for(|stopping| *stopping) => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                // Usually EMFILE; do not spin.
                warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        match slots.clone().try_acquire_owned() {
            Ok(permit) => {
                let session = Session::new(stream, peer, shared.clone());
                let stop = stop.clone();
                tokio::spawn(async move {
                    session.run(stop).await;
                    drop(permit);
                });
            }
            Err(_) => {
                warn!(%peer, "connection limit reached; refusing connection");
                let hostname = shared.config.smtp.hostname.clone();
                tokio::spawn(async move {
                    let mut stream = stream;
                    let line =
                        format!("421 4.7.0 {hostname} Too many connections, try again later\r\n");
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        stream.write_all(line.as_bytes()),
                    )
                    .await;
                });
            }
        }
    }
}

enum LineRead {
    /// A complete line is in the buffer (terminator included).
    Line,
    /// The line exceeded the limit; it was consumed and discarded.
    TooLong,
    /// The peer closed the connection.
    Eof,
}

/// Reads one `\n`-terminated line into `buf` (cleared first), never holding
/// more than `limit` bytes of it.
async fn read_line_limited<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> io::Result<LineRead> {
    buf.clear();
    let mut too_long = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(LineRead::Eof);
        }
        let (used, complete) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !too_long {
            if buf.len() + used > limit {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&available[..used]);
            }
        }
        reader.consume(used);
        if complete {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

fn strip_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Parses the `<path> [params]` part of MAIL FROM / RCPT TO. Bare addresses
/// without angle brackets are tolerated; source routes are dropped.
fn parse_path(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let (address, params) = if let Some(inner) = s.strip_prefix('<') {
        let end = inner.find('>')?;
        (&inner[..end], &inner[end + 1..])
    } else {
        s.split_once(' ').unwrap_or((s, ""))
    };
    let address = match address.strip_prefix('@') {
        Some(route) => route.split_once(':')?.1,
        None => address,
    };
    if address.len() > 256
        || address
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
    {
        return None;
    }
    Some((address.to_string(), params.trim()))
}

/// Whether an authenticated `user` may send as `sender`: the part before
/// the last `@` must be the user name, ASCII case ignored, at any domain.
/// A user name that is itself an address must be the whole sender. The
/// null sender belongs to nobody.
fn sender_owned_by(sender: &str, user: &str) -> bool {
    if user.contains('@') {
        return sender.eq_ignore_ascii_case(user);
    }
    match sender.rsplit_once('@') {
        Some((local, domain)) => !domain.is_empty() && local.eq_ignore_ascii_case(user),
        None => false,
    }
}

enum Flow {
    Continue,
    Close,
}

struct Session {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer: SocketAddr,
    shared: Arc<Shared>,
    line: Vec<u8>,
    esmtp: bool,
    auth_offered: bool,
    auth_user: Option<String>,
    auth_failures: u32,
    unrecognized: u32,
    mail_from: Option<String>,
    rcpt_to: Vec<String>,
}

impl Session {
    fn new(stream: TcpStream, peer: SocketAddr, shared: Arc<Shared>) -> Self {
        let _ = stream.set_nodelay(true);
        let (read, write) = stream.into_split();
        let smtp = &shared.config.smtp;
        let auth_offered = !smtp.credentials.is_empty()
            && ip_in_networks(peer.ip(), &smtp.plaintext_auth_networks);
        Self {
            reader: BufReader::new(read),
            writer: write,
            peer,
            line: Vec::with_capacity(512),
            esmtp: false,
            auth_offered,
            auth_user: None,
            auth_failures: 0,
            unrecognized: 0,
            mail_from: None,
            rcpt_to: Vec::new(),
            shared,
        }
    }

    fn config(&self) -> &Config {
        &self.shared.config
    }

    async fn run(mut self, mut stop: watch::Receiver<bool>) {
        debug!(peer = %self.peer, "connection opened");
        match self.serve(&mut stop).await {
            Ok(()) => debug!(peer = %self.peer, "connection closed"),
            Err(e) => debug!(peer = %self.peer, "connection closed: {e}"),
        }
        let _ = self.writer.shutdown().await;
    }

    async fn write(&mut self, text: String) -> io::Result<()> {
        let timeout = self.config().smtp.timeout;
        match tokio::time::timeout(timeout, self.writer.write_all(text.as_bytes())).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out")),
        }
    }

    async fn reply(&mut self, code: u16, text: &str) -> io::Result<()> {
        self.write(format!("{code} {text}\r\n")).await
    }

    async fn reply_multiline(&mut self, code: u16, lines: &[String]) -> io::Result<()> {
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            let sep = if i + 1 == lines.len() { ' ' } else { '-' };
            out.push_str(&format!("{code}{sep}{line}\r\n"));
        }
        self.write(out).await
    }

    /// Reads a line with the idle timeout. `Ok(None)` means the connection
    /// should close (EOF or timeout, already answered where possible).
    async fn read_line(&mut self, limit: usize) -> io::Result<Option<LineRead>> {
        let timeout = self.config().smtp.timeout;
        match tokio::time::timeout(
            timeout,
            read_line_limited(&mut self.reader, &mut self.line, limit),
        )
        .await
        {
            Ok(Ok(LineRead::Eof)) => Ok(None),
            Ok(Ok(read)) => Ok(Some(read)),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                debug!(peer = %self.peer, "idle timeout");
                self.reply(421, "4.4.2 Idle timeout, closing connection")
                    .await?;
                Ok(None)
            }
        }
    }

    fn reset_transaction(&mut self) {
        self.mail_from = None;
        self.rcpt_to.clear();
    }

    async fn serve(&mut self, stop: &mut watch::Receiver<bool>) -> io::Result<()> {
        let greeting = format!("{} ESMTP smtp_to_telegram", self.config().smtp.hostname);
        self.reply(220, &greeting).await?;
        loop {
            if *stop.borrow() {
                return self.reply(421, "4.3.2 Service shutting down").await;
            }
            let timeout = self.config().smtp.timeout;
            // Waiting for a command is the only point where shutdown
            // interrupts a session; DATA and deliveries run to completion.
            let read = tokio::select! {
                read = tokio::time::timeout(
                    timeout,
                    read_line_limited(&mut self.reader, &mut self.line, MAX_COMMAND_LINE),
                ) => Some(read),
                _ = stop.wait_for(|stopping| *stopping) => None,
            };
            let read = match read {
                None => return self.reply(421, "4.3.2 Service shutting down").await,
                Some(Err(_)) => {
                    debug!(peer = %self.peer, "idle timeout");
                    return self
                        .reply(421, "4.4.2 Idle timeout, closing connection")
                        .await;
                }
                Some(Ok(read)) => read?,
            };
            let flow = match read {
                LineRead::Eof => return Ok(()),
                LineRead::TooLong => {
                    self.unrecognized += 1;
                    self.reply(500, "5.5.2 Line too long").await?;
                    if self.unrecognized >= MAX_UNRECOGNIZED_COMMANDS {
                        self.reply(554, "5.5.1 Too many errors, closing connection")
                            .await?;
                        Flow::Close
                    } else {
                        Flow::Continue
                    }
                }
                LineRead::Line => {
                    let line = String::from_utf8_lossy(strip_line_ending(&self.line)).into_owned();
                    self.command(&line).await?
                }
            };
            if let Flow::Close = flow {
                return Ok(());
            }
        }
    }

    async fn command(&mut self, line: &str) -> io::Result<Flow> {
        let (verb, arg) = match line.split_once(' ') {
            Some((verb, arg)) => (verb, arg.trim()),
            None => (line.trim(), ""),
        };
        let verb = verb.to_ascii_uppercase();
        match verb.as_str() {
            "HELO" | "EHLO" => self.helo(&verb, arg).await?,
            "MAIL" => self.mail(arg).await?,
            "RCPT" => self.rcpt(arg).await?,
            "DATA" => return self.data().await,
            "AUTH" => return self.auth(arg).await,
            "RSET" => {
                self.reset_transaction();
                self.reply(250, "2.0.0 OK").await?;
            }
            "NOOP" => self.reply(250, "2.0.0 OK").await?,
            "VRFY" => {
                self.reply(252, "2.5.0 Cannot VRFY user, but will accept message")
                    .await?
            }
            "HELP" => {
                self.reply(
                    214,
                    "2.0.0 Commands: HELO EHLO AUTH MAIL RCPT DATA RSET NOOP VRFY HELP QUIT",
                )
                .await?
            }
            "QUIT" => {
                self.reply(221, "2.0.0 Bye").await?;
                return Ok(Flow::Close);
            }
            _ => {
                self.unrecognized += 1;
                if self.unrecognized >= MAX_UNRECOGNIZED_COMMANDS {
                    self.reply(
                        554,
                        "5.5.1 Too many unrecognized commands, closing connection",
                    )
                    .await?;
                    return Ok(Flow::Close);
                }
                self.reply(500, "5.5.2 Command unrecognized").await?;
            }
        }
        Ok(Flow::Continue)
    }

    async fn helo(&mut self, verb: &str, arg: &str) -> io::Result<()> {
        if arg.is_empty() {
            return self
                .reply(501, &format!("5.5.4 Syntax: {verb} hostname"))
                .await;
        }
        self.reset_transaction();
        let hostname = self.config().smtp.hostname.clone();
        let arg: String = arg.chars().filter(|c| !c.is_control()).collect();
        if verb == "HELO" {
            self.esmtp = false;
            return self.reply(250, &format!("{hostname} Hello {arg}")).await;
        }
        self.esmtp = true;
        let mut lines = vec![
            format!("{hostname} Hello {arg}"),
            format!("SIZE {}", self.config().smtp.max_message_size),
            "8BITMIME".to_string(),
            "PIPELINING".to_string(),
            "ENHANCEDSTATUSCODES".to_string(),
        ];
        if self.auth_offered {
            lines.push("AUTH PLAIN LOGIN".to_string());
        }
        lines.push("HELP".to_string());
        self.reply_multiline(250, &lines).await
    }

    async fn mail(&mut self, arg: &str) -> io::Result<()> {
        let Some(rest) = strip_prefix_ignore_case(arg, "FROM:") else {
            return self.reply(501, "5.5.4 Syntax: MAIL FROM:<address>").await;
        };
        if self.auth_user.is_none() && !self.config().smtp.allow_anonymous {
            warn!(peer = %self.peer, "rejected unauthenticated MAIL FROM");
            return self.reply(530, "5.7.0 Authentication required").await;
        }
        if self.mail_from.is_some() {
            return self.reply(503, "5.5.1 Nested MAIL command").await;
        }
        let Some((address, params)) = parse_path(rest) else {
            return self.reply(501, "5.1.7 Bad sender address syntax").await;
        };
        // An authenticated client sends as itself; anonymous ones are not
        // checked (with --allow-anonymous their sender proves nothing).
        if let Some(user) = self.auth_user.as_deref() {
            if !sender_owned_by(&address, user) {
                let text = format!("5.7.1 <{address}>: Sender address not owned by user {user}");
                warn!(peer = %self.peer, user = %user, from = %address, "rejected MAIL FROM not owned by the authenticated user");
                return self.reply(553, &text).await;
            }
        }
        for param in params.split_whitespace() {
            if let Some(size) = strip_prefix_ignore_case(param, "SIZE=") {
                match size.parse::<u64>() {
                    Ok(size) if size > self.config().smtp.max_message_size => {
                        return self
                            .reply(552, "5.3.4 Message size exceeds fixed maximum message size")
                            .await;
                    }
                    Ok(_) => {}
                    Err(_) => return self.reply(501, "5.5.4 Invalid SIZE parameter").await,
                }
            }
        }
        self.mail_from = Some(address);
        self.reply(250, "2.1.0 OK").await
    }

    async fn rcpt(&mut self, arg: &str) -> io::Result<()> {
        let Some(rest) = strip_prefix_ignore_case(arg, "TO:") else {
            return self.reply(501, "5.5.4 Syntax: RCPT TO:<address>").await;
        };
        if self.mail_from.is_none() {
            return self.reply(503, "5.5.1 Need MAIL command first").await;
        }
        if self.rcpt_to.len() >= MAX_RECIPIENTS {
            return self.reply(452, "4.5.3 Too many recipients").await;
        }
        match parse_path(rest) {
            Some((address, _)) if !address.is_empty() => {
                self.rcpt_to.push(address);
                self.reply(250, "2.1.5 OK").await
            }
            _ => self.reply(501, "5.1.3 Bad recipient address syntax").await,
        }
    }

    /// Reads the message after `354`. `Ok(None)`: the connection must close.
    async fn read_data(&mut self) -> io::Result<Option<Result<Vec<u8>, ()>>> {
        let max = usize::try_from(self.config().smtp.max_message_size).unwrap_or(usize::MAX);
        let mut data = Vec::new();
        let mut too_large = false;
        loop {
            // Allow a line to reach just past the limit so an oversized
            // message is detected rather than cut.
            let limit = max.saturating_sub(data.len()).saturating_add(1024);
            match self.read_line(limit).await? {
                None => return Ok(None),
                Some(LineRead::TooLong) => {
                    too_large = true;
                    data = Vec::new();
                }
                Some(LineRead::Eof) => return Ok(None),
                Some(LineRead::Line) => {
                    let line = &self.line[..];
                    if line == b".\r\n" || line == b".\n" {
                        break;
                    }
                    let line = line.strip_prefix(b".").unwrap_or(line);
                    // Store lines with bare LF endings, as Go's textproto
                    // DotReader did, so the text sent to Telegram has no CRs.
                    let line = line.strip_suffix(b"\r\n").unwrap_or(line);
                    let line = line.strip_suffix(b"\n").unwrap_or(line);
                    if !too_large {
                        if data.len() + line.len() + 1 > max {
                            too_large = true;
                            data = Vec::new();
                        } else {
                            data.extend_from_slice(line);
                            data.push(b'\n');
                        }
                    }
                }
            }
        }
        Ok(Some(if too_large { Err(()) } else { Ok(data) }))
    }

    async fn data(&mut self) -> io::Result<Flow> {
        let Some(mail_from) = self.mail_from.clone() else {
            self.reply(503, "5.5.1 Need MAIL command first").await?;
            return Ok(Flow::Continue);
        };
        if self.rcpt_to.is_empty() {
            self.reply(503, "5.5.1 Need RCPT command first").await?;
            return Ok(Flow::Continue);
        }
        self.reply(354, "End data with <CR><LF>.<CR><LF>").await?;
        let data = match self.read_data().await? {
            None => return Ok(Flow::Close),
            Some(Err(())) => {
                self.reset_transaction();
                warn!(peer = %self.peer, "rejected message: larger than the size limit");
                self.reply(552, "5.3.4 Message size exceeds fixed maximum message size")
                    .await?;
                return Ok(Flow::Continue);
            }
            Some(Ok(data)) => data,
        };

        let rcpt_to = std::mem::take(&mut self.rcpt_to);
        self.reset_transaction();
        let auth = self
            .auth_user
            .clone()
            .unwrap_or_else(|| "(anonymous)".to_string());
        let envelope = Envelope {
            mail_from: &mail_from,
            rcpt_to: &rcpt_to,
            data: &data,
        };
        let shared = self.shared.clone();
        let result = delivery::deliver(
            &envelope,
            &shared.config.format,
            &shared.config.telegram,
            &shared.telegram,
        )
        .await;
        match result {
            Ok(report) => {
                info!(
                    peer = %self.peer,
                    auth = %auth,
                    from = %mail_from,
                    to = %rcpt_to.join(","),
                    bytes = data.len(),
                    chats = report.chats,
                    attachments = report.attachments_sent,
                    attachments_failed = report.attachments_failed,
                    "message forwarded to Telegram"
                );
                self.reply(
                    250,
                    &format!("2.0.0 OK: forwarded to {} chat(s)", report.chats),
                )
                .await?;
            }
            Err(e) => {
                error!(
                    peer = %self.peer,
                    auth = %auth,
                    from = %mail_from,
                    to = %rcpt_to.join(","),
                    bytes = data.len(),
                    "message not forwarded: {e}"
                );
                self.reply(451, &format!("4.3.0 Error: {e}")).await?;
            }
        }
        Ok(Flow::Continue)
    }

    /// Reads one AUTH continuation line. `Ok(None)`: close the connection.
    async fn read_auth_response(&mut self) -> io::Result<Option<String>> {
        match self.read_line(MAX_COMMAND_LINE).await? {
            None => Ok(None),
            Some(LineRead::Line) => Ok(Some(
                String::from_utf8_lossy(strip_line_ending(&self.line))
                    .trim()
                    .to_string(),
            )),
            Some(_) => Ok(Some(String::new())),
        }
    }

    async fn auth(&mut self, arg: &str) -> io::Result<Flow> {
        if self.auth_user.is_some() {
            self.reply(503, "5.5.1 Already authenticated").await?;
            return Ok(Flow::Continue);
        }
        if self.mail_from.is_some() {
            self.reply(503, "5.5.1 AUTH not permitted during a mail transaction")
                .await?;
            return Ok(Flow::Continue);
        }
        if self.config().smtp.credentials.is_empty() {
            self.reply(502, "5.5.1 AUTH not available").await?;
            return Ok(Flow::Continue);
        }
        if !self.esmtp {
            self.reply(503, "5.5.1 Send EHLO first").await?;
            return Ok(Flow::Continue);
        }
        if !self.auth_offered {
            warn!(peer = %self.peer, "refused AUTH from outside the plaintext AUTH networks");
            self.reply(
                538,
                "5.7.11 Encryption required for requested authentication mechanism",
            )
            .await?;
            return Ok(Flow::Continue);
        }

        let (mechanism, initial) = match arg.split_once(' ') {
            Some((m, rest)) => (m.to_ascii_uppercase(), Some(rest.trim().to_string())),
            None => (arg.to_ascii_uppercase(), None),
        };
        let credentials = match mechanism.as_str() {
            "PLAIN" => {
                let response = match initial {
                    Some(response) => response,
                    None => {
                        self.reply(334, "").await?;
                        match self.read_auth_response().await? {
                            Some(response) => response,
                            None => return Ok(Flow::Close),
                        }
                    }
                };
                if response == "*" {
                    self.reply(501, "5.7.0 Authentication cancelled").await?;
                    return Ok(Flow::Continue);
                }
                let decoded = if response == "=" {
                    Ok(Vec::new())
                } else {
                    BASE64.decode(response.as_bytes())
                };
                match decoded {
                    Ok(decoded) => decode_plain(&decoded),
                    Err(_) => {
                        self.reply(501, "5.5.2 Invalid base64 data").await?;
                        return Ok(Flow::Continue);
                    }
                }
            }
            "LOGIN" => {
                let user = match initial {
                    Some(user) => user,
                    None => {
                        self.reply(334, "VXNlcm5hbWU6").await?;
                        match self.read_auth_response().await? {
                            Some(user) => user,
                            None => return Ok(Flow::Close),
                        }
                    }
                };
                if user == "*" {
                    self.reply(501, "5.7.0 Authentication cancelled").await?;
                    return Ok(Flow::Continue);
                }
                self.reply(334, "UGFzc3dvcmQ6").await?;
                let password = match self.read_auth_response().await? {
                    Some(password) => password,
                    None => return Ok(Flow::Close),
                };
                if password == "*" {
                    self.reply(501, "5.7.0 Authentication cancelled").await?;
                    return Ok(Flow::Continue);
                }
                let decode = |s: &str| {
                    BASE64
                        .decode(s.as_bytes())
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                };
                match (decode(&user), decode(&password)) {
                    (Some(user), Some(password)) => Some((user, password)),
                    _ => {
                        self.reply(501, "5.5.2 Invalid base64 data").await?;
                        return Ok(Flow::Continue);
                    }
                }
            }
            _ => {
                self.reply(504, "5.5.4 Unrecognized authentication mechanism")
                    .await?;
                return Ok(Flow::Continue);
            }
        };

        let accepted = match &credentials {
            Some((user, password)) => self.config().smtp.credentials.verify(user, password),
            None => false,
        };
        if accepted {
            let user = credentials.map(|(user, _)| user).unwrap_or_default();
            info!(peer = %self.peer, user = %user, mechanism = %mechanism, "authenticated");
            self.auth_user = Some(user);
            self.reply(235, "2.7.0 Authentication successful").await?;
            return Ok(Flow::Continue);
        }

        self.auth_failures += 1;
        let user = credentials.map(|(user, _)| user).unwrap_or_default();
        warn!(peer = %self.peer, user = %user, mechanism = %mechanism, "authentication failed");
        if self.auth_failures >= MAX_AUTH_FAILURES {
            self.reply(
                421,
                "4.7.0 Too many authentication failures, closing connection",
            )
            .await?;
            return Ok(Flow::Close);
        }
        self.reply(535, "5.7.8 Authentication credentials invalid")
            .await?;
        Ok(Flow::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths() {
        assert_eq!(parse_path("<a@b>"), Some(("a@b".into(), "")));
        assert_eq!(
            parse_path(" <a@b> SIZE=10 BODY=8BITMIME"),
            Some(("a@b".into(), "SIZE=10 BODY=8BITMIME"))
        );
        assert_eq!(parse_path("<>"), Some((String::new(), "")));
        assert_eq!(parse_path("a@b"), Some(("a@b".into(), "")));
        assert_eq!(parse_path("<@relay:a@b>"), Some(("a@b".into(), "")));
        assert_eq!(parse_path("<a b@c>"), None);
        assert_eq!(parse_path("<a@b"), None);
    }

    #[test]
    fn senders_owned_by_a_user() {
        assert!(sender_owned_by("alert@X-Linode.x.julycat.com", "alert"));
        assert!(sender_owned_by("Alert@example.org", "alert"));
        assert!(!sender_owned_by("alerts-test@julycat.com", "alert"));
        assert!(!sender_owned_by("x@alert@y", "alert"));
        assert!(!sender_owned_by("alert", "alert"));
        assert!(!sender_owned_by("alert@", "alert"));
        assert!(!sender_owned_by("", "alert"));
        // A user name that is an address owns exactly that address.
        assert!(sender_owned_by("alice@sclx.me", "alice@sclx.me"));
        assert!(sender_owned_by("alice@SCLX.ME", "alice@sclx.me"));
        assert!(!sender_owned_by("alice@other.org", "alice@sclx.me"));
    }

    #[test]
    fn strips_prefixes_case_insensitively() {
        assert_eq!(strip_prefix_ignore_case("from:<x>", "FROM:"), Some("<x>"));
        assert_eq!(strip_prefix_ignore_case("FRO", "FROM:"), None);
        assert_eq!(strip_prefix_ignore_case("FRÖM:", "FROM:"), None);
    }

    #[tokio::test]
    async fn ipv4_and_ipv6_wildcards_can_share_a_port() {
        let v4 = bind_listener("0.0.0.0:0".parse().unwrap()).unwrap();
        let port = v4.local_addr().unwrap().port();
        match bind_listener(SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port))) {
            Ok(v6) => assert_eq!(v6.local_addr().unwrap().port(), port),
            // EAFNOSUPPORT / EADDRNOTAVAIL: no IPv6 here (some build
            // sandboxes), so there is nothing to collide with.
            Err(e) if matches!(e.raw_os_error(), Some(97 | 99)) => {
                eprintln!("skipped, no IPv6: {e}")
            }
            Err(e) => panic!("binding [::]:{port} next to 0.0.0.0:{port} failed: {e}"),
        }
    }

    #[tokio::test]
    async fn reads_limited_lines() {
        let input: &[u8] = b"short\r\nthis line is far too long\r\nok\n";
        let mut reader = BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_limited(&mut reader, &mut buf, 10).await.unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"short\r\n");
        assert!(matches!(
            read_line_limited(&mut reader, &mut buf, 10).await.unwrap(),
            LineRead::TooLong
        ));
        assert!(matches!(
            read_line_limited(&mut reader, &mut buf, 10).await.unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"ok\n");
        assert!(matches!(
            read_line_limited(&mut reader, &mut buf, 10).await.unwrap(),
            LineRead::Eof
        ));
    }
}
