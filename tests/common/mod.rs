//! Shared test harness: the real SMTP server running in-process on an
//! ephemeral port, a mock Telegram Bot API, and SMTP clients (lettre, plus a
//! raw line-level client for protocol details lettre does not expose).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Form, FromRequest, Multipart, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use lettre::transport::smtp::authentication::{Credentials as SmtpCredentials, Mechanism};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use smtp_to_telegram::auth::Credentials;
use smtp_to_telegram::{Config, FormatConfig, SmtpConfig, TelegramConfig};

/// An obviously fake bot token.
pub const TOKEN: &str = "42:ZZZ";
pub const CHAT_IDS: [&str; 2] = ["42", "142"];
pub const USER: &str = "alice";
pub const PASSWORD: &str = "correct horse";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFile {
    pub field: String,
    pub filename: String,
    pub content_type: String,
    pub content: Vec<u8>,
}

/// One Bot API request as the mock received it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub token: String,
    pub method: String,
    pub fields: BTreeMap<String, String>,
    pub file: Option<RecordedFile>,
}

impl Recorded {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

#[derive(Default)]
struct MockState {
    requests: Mutex<Vec<Recorded>>,
    /// Methods that answer HTTP 400.
    failing: Mutex<Vec<String>>,
    next_id: Mutex<i64>,
}

pub struct MockTelegram {
    pub prefix: String,
    state: Arc<MockState>,
}

impl MockTelegram {
    pub async fn start() -> Self {
        let state = Arc::new(MockState {
            next_id: Mutex::new(1000),
            ..MockState::default()
        });
        let app = Router::new()
            .route("/{bot}/{method}", post(handle))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            prefix: format!("http://{addr}/"),
            state,
        }
    }

    pub fn fail(&self, method: &str) {
        self.state.failing.lock().unwrap().push(method.to_string());
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn messages(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "sendMessage")
            .collect()
    }

    pub fn files(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != "sendMessage")
            .collect()
    }
}

async fn handle(
    State(state): State<Arc<MockState>>,
    Path((bot, method)): Path<(String, String)>,
    request: Request,
) -> impl IntoResponse {
    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    let mut fields = BTreeMap::new();
    let mut file = None;
    if is_multipart {
        let mut multipart = Multipart::from_request(request, &()).await.unwrap();
        while let Some(field) = multipart.next_field().await.unwrap() {
            let name = field.name().unwrap_or_default().to_string();
            match field.file_name().map(str::to_string) {
                Some(filename) => {
                    let content_type = field.content_type().unwrap_or_default().to_string();
                    let content = field.bytes().await.unwrap().to_vec();
                    file = Some(RecordedFile {
                        field: name,
                        filename,
                        content_type,
                        content,
                    });
                }
                None => {
                    fields.insert(name, field.text().await.unwrap());
                }
            }
        }
    } else {
        let Form(form): Form<Vec<(String, String)>> =
            Form::from_request(request, &()).await.unwrap();
        fields.extend(form);
    }
    state.requests.lock().unwrap().push(Recorded {
        token: bot.strip_prefix("bot").unwrap_or(&bot).to_string(),
        method: method.clone(),
        fields,
        file,
    });
    if state.failing.lock().unwrap().contains(&method) {
        return (
            StatusCode::BAD_REQUEST,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: mock failure"}"#
                .to_string(),
        );
    }
    let id = {
        let mut next = state.next_id.lock().unwrap();
        *next += 1;
        *next
    };
    (
        StatusCode::OK,
        format!(r#"{{"ok":true,"result":{{"message_id":{id},"chat":{{"id":1}}}}}}"#),
    )
}

/// Defaults for tests: anonymous delivery on, no credentials, two chats.
pub fn config(api_prefix: &str) -> Config {
    Config {
        smtp: SmtpConfig {
            listen: Vec::new(),
            hostname: "testhost".to_string(),
            allow_anonymous: true,
            timeout: Duration::from_secs(10),
            ..SmtpConfig::default()
        },
        telegram: TelegramConfig {
            bot_token: TOKEN.to_string(),
            chat_ids: CHAT_IDS.iter().map(|s| s.to_string()).collect(),
            api_prefix: api_prefix.to_string(),
            timeout: Duration::from_secs(5),
            ..TelegramConfig::default()
        },
        format: FormatConfig::default(),
    }
}

/// `config` with credentials for USER/PASSWORD and the given anonymous mode.
pub fn auth_config(api_prefix: &str, allow_anonymous: bool) -> Config {
    let mut config = config(api_prefix);
    config.smtp.allow_anonymous = allow_anonymous;
    config.smtp.credentials = Credentials::from_pairs([(USER, PASSWORD)]);
    config
}

pub struct TestServer {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl TestServer {
    pub async fn start(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = tokio::spawn(smtp_to_telegram::serve(
            Arc::new(config),
            vec![listener],
            async move {
                let _ = stopped.await;
            },
        ));
        Self {
            addr,
            stop: Some(stop),
            handle,
        }
    }

    /// Triggers a graceful shutdown and waits for `serve` to return.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), &mut self.handle)
            .await
            .expect("serve returned within 10s")
            .expect("serve did not panic")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub fn mailer(
    addr: SocketAddr,
    credentials: Option<(&str, &str)>,
    mechanism: Mechanism,
) -> AsyncSmtpTransport<Tokio1Executor> {
    let mut builder =
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(addr.ip().to_string())
            .port(addr.port())
            .hello_name(ClientId::Domain("client.test".to_string()))
            .timeout(Some(Duration::from_secs(10)));
    if let Some((user, password)) = credentials {
        builder = builder
            .credentials(SmtpCredentials::new(user.to_string(), password.to_string()))
            .authentication(vec![mechanism]);
    }
    builder.build()
}

pub fn simple_message(subject: &str, body: &str) -> Message {
    message_from("from@test", subject, body)
}

/// A sender USER owns, which an authenticated client has to use.
pub const USER_ADDRESS: &str = "alice@test";

pub fn message_from(from: &str, subject: &str, body: &str) -> Message {
    Message::builder()
        .from(from.parse().unwrap())
        .to("to@test".parse().unwrap())
        .subject(subject)
        .body(body.to_string())
        .unwrap()
}

/// Sends `message` and returns the SMTP outcome as text: `Ok(())` or the
/// error with its reply code.
pub async fn send(
    addr: SocketAddr,
    credentials: Option<(&str, &str)>,
    mechanism: Mechanism,
    message: Message,
) -> Result<(), String> {
    mailer(addr, credentials, mechanism)
        .send(message)
        .await
        .map(|_| ())
        .map_err(|e| {
            let code = e.status().map(|c| c.to_string()).unwrap_or_default();
            format!("{code} {e}")
        })
}

/// A line-level SMTP client.
pub struct RawClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl RawClient {
    /// Connects and checks the 220 greeting.
    pub async fn connect(addr: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (read, write) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        let (code, text) = client.reply().await;
        assert_eq!(code, 220, "greeting: {text}");
        client
    }

    /// Connects and returns the first reply without checking it.
    pub async fn connect_raw(addr: SocketAddr) -> (Self, u16, String) {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (read, write) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        let (code, text) = client.reply().await;
        (client, code, text)
    }

    pub async fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
    }

    /// Reads one (possibly multi-line) reply; lines are joined with '\n'.
    /// Returns code 0 if the connection was closed.
    pub async fn reply(&mut self) -> (u16, String) {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.reader.read_line(&mut line))
                .await
                .expect("reply within 10s")
                .unwrap();
            if n == 0 {
                return (0, lines.join("\n"));
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            assert!(line.len() >= 3, "short reply line {line:?}");
            let code: u16 = line[..3].parse().expect("numeric reply code");
            let last = line.as_bytes().get(3) != Some(&b'-');
            lines.push(line.get(4..).unwrap_or_default().to_string());
            if last {
                return (code, lines.join("\n"));
            }
        }
    }

    pub async fn cmd(&mut self, line: &str) -> (u16, String) {
        self.write(format!("{line}\r\n").as_bytes()).await;
        self.reply().await
    }

    /// True once the server has closed the connection.
    pub async fn is_closed(&mut self) -> bool {
        let mut line = String::new();
        matches!(
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line)).await,
            Ok(Ok(0))
        )
    }
}

pub fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}
