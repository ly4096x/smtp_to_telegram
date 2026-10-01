# SMTP to Telegram

`smtp_to_telegram` is a small SMTP server that forwards every email it
receives to one or more Telegram chats. Point anything that can send
notification mail (routers, NAS boxes, cron, monitoring) at it and the mail
shows up in Telegram.

This branch is a Rust rewrite of the Go program
([KostyaEsmukov/smtp_to_telegram](https://github.com/KostyaEsmukov/smtp_to_telegram),
and this fork's MarkdownV2 additions). It keeps the Go version's features —
including the attachment and long-message handling the upstream project added
later — and adds SMTP authentication. The configuration is not compatible
with the Go version; see [Migrating from the Go version](#migrating-from-the-go-version).

## Quick start

1. Create a bot with [@BotFather](https://core.telegram.org/bots#how-do-i-create-a-bot)
   and note its token.
2. In every Telegram account or group that should get the mail, open the bot
   and press `/start`.
3. Find the chat ids: `curl https://api.telegram.org/bot<TOKEN>/getUpdates`.
4. Run it:

   ```sh
   nix build github:ly4096x/smtp_to_telegram/rust     # or: cargo build --release
   printf '%s' '<TOKEN>' > bot-token
   ./result/bin/smtp_to_telegram \
       --telegram-bot-token-file bot-token \
       --telegram-chat-ids 123456789,-1001234567890 \
       --allow-anonymous
   ```

5. Send a test mail, for example with `swaks --server 127.0.0.1:2525 --to you@example.com`.

## Configuration

Every option is a command line flag, and every flag has an environment
variable that is used when the flag is absent. Run `smtp_to_telegram --help`
for the full list.

### Secrets

Secrets are **never** accepted as command line arguments, because any local
user can read those with `ps`. Each has a file option (meant for systemd
`LoadCredential=`) and an environment variable; giving both is an error.

| Secret | File | Environment variable |
| --- | --- | --- |
| Telegram bot token | `--telegram-bot-token-file PATH` | `ST_TELEGRAM_BOT_TOKEN` |
| SMTP credentials | `--credentials-file PATH` | `ST_SMTP_CREDENTIALS` |

The credentials format is one `username:password` per line. The password is
everything after the first `:`, taken verbatim (spaces included); empty lines
and lines starting with `#` are ignored. Prefer the files: an environment
variable is visible in `/proc/<pid>/environ` to root and to the same user.

### SMTP

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--smtp-listen ADDR` | `ST_SMTP_LISTEN` | `127.0.0.1:2525` | Address to listen on; repeat the flag or separate with commas for several. |
| `--smtp-primary-host NAME` | `ST_SMTP_PRIMARY_HOST` | system host name | Name in the greeting and the EHLO reply. |
| `--smtp-max-envelope-size SIZE` | `ST_SMTP_MAX_ENVELOPE_SIZE` | `50m` | Largest accepted message (`10m` = 10 000 000 bytes, `4MiB` = 4 194 304). |
| `--smtp-max-connections N` | `ST_SMTP_MAX_CONNECTIONS` | `100` | Simultaneous connections; extra ones get `421`. |
| `--smtp-timeout-seconds N` | `ST_SMTP_TIMEOUT_SECONDS` | `30` | Idle time allowed for each command or line of data. |
| `--allow-anonymous` | `ST_SMTP_ALLOW_ANONYMOUS` (`true`/`false`) | off | Accept mail from clients that did not authenticate. |
| `--credentials-file PATH` | `ST_SMTP_CREDENTIALS_FILE` | none | Users for `AUTH PLAIN` and `AUTH LOGIN`. |
| `--plaintext-auth-networks CIDR` | `ST_SMTP_PLAINTEXT_AUTH_NETWORKS` | `127.0.0.0/8,::1/128` | Client networks to which AUTH is offered (see below). |

### Telegram

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--telegram-chat-ids LIST` | `ST_TELEGRAM_CHAT_IDS` | required | Comma-separated chat ids. |
| `--telegram-chat-ids-file PATH` | `ST_TELEGRAM_CHAT_IDS_FILE` | | The same, from a file (commas or whitespace). Use one of the two. |
| `--telegram-api-prefix URL` | `ST_TELEGRAM_API_PREFIX` | `https://api.telegram.org/` | Requests go to `{prefix}bot{token}/{method}`. |
| `--telegram-api-timeout-seconds S` | `ST_TELEGRAM_API_TIMEOUT_SECONDS` | `30` | Timeout of each Bot API request. |
| `--telegram-api-parse-mode MODE` | `ST_TELEGRAM_API_PARSE_MODE` | `none` | `none`, `MarkdownV2`, `HTML` or `Markdown` (case-insensitive). |
| `--telegram-api-extra-param K=V` | `ST_TELEGRAM_API_EXTRA_PARAMS` (comma-separated) | | Extra Bot API parameter for every request, e.g. `message_thread_id=42` or `disable_notification=true`. |

`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` (including `socks5://`) and
`NO_PROXY` are honoured. TLS uses rustls and trusts the system's CA
certificates (`SSL_CERT_FILE` and `SSL_CERT_DIR` are honoured); when the
system has none, a bundled copy of Mozilla's roots is used instead. The
startup log says which.

### Message

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--message-template T` | `ST_TELEGRAM_MESSAGE_TEMPLATE` | see below | The message template. |
| `--message-template-file PATH` | `ST_TELEGRAM_MESSAGE_TEMPLATE_FILE` | | The template from a file. |
| `--message-length-to-send-as-file N` | `ST_MESSAGE_LENGTH_TO_SEND_AS_FILE` | `4095` | Longer messages are truncated and the full text is attached. 1–4096. |
| `--forwarded-attachment-max-size SIZE` | `ST_FORWARDED_ATTACHMENT_MAX_SIZE` | `10m` | Largest attachment sent as a document; `0` disables. Telegram's limit is 50 MB. |
| `--forwarded-attachment-max-photo-size SIZE` | `ST_FORWARDED_ATTACHMENT_MAX_PHOTO_SIZE` | `10m` | Largest JPEG/PNG sent as a photo; `0` disables. Telegram's limit is 10 MB. |
| `--forwarded-attachment-respect-errors` | `ST_FORWARDED_ATTACHMENT_RESPECT_ERRORS` | off | Fail the whole mail if an attachment cannot be sent. |
| `--log-level LEVEL` | `ST_LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` or `trace`. Logs go to stderr. |

## Authentication rules

There is no TLS, so a password sent with AUTH crosses the network in the
clear. The rules follow from that:

- `AUTH PLAIN` and `AUTH LOGIN` (with or without an initial response) are
  **advertised and accepted only when credentials are configured and the
  client's address is inside `--plaintext-auth-networks`**. The default is
  loopback only; list the networks you trust or that are already encrypted,
  such as VPN tunnels. IPv4-mapped IPv6 clients match IPv4 networks.
- A client outside those networks does not see AUTH in the EHLO reply, and an
  AUTH command gets `538 5.7.11 Encryption required`.
- Without `--allow-anonymous`, `MAIL FROM` before a successful AUTH gets
  `530 5.7.0 Authentication required`.
- With `--allow-anonymous`, both authenticated and anonymous clients can send.
  A client that does try AUTH must still get the password right.
- A wrong user or password gets `535 5.7.8`; the third failure on one
  connection closes it with `421`. Passwords are compared without stopping
  at the first differing byte.
- AUTH is accepted once per session, after EHLO and outside a mail
  transaction. A PLAIN authorization identity must be empty or equal to the
  user name.
- With neither credentials nor `--allow-anonymous` nobody could send, so the
  program refuses to start.

Which interfaces the server listens on is a separate matter: use
`--smtp-listen` and the firewall for that.

## How mail becomes a Telegram message

The default template is

```
From: {from}\nTo: {to}\nSubject: {subject}\n\n{body}\n\n{attachments_details}
```

- `{from}`: the envelope sender (`MAIL FROM`), empty for `<>`.
- `{to}`: every envelope recipient (`RCPT TO`), joined with `, `.
- `{subject}`: the decoded `Subject` header.
- `{body}`: the text body, with surrounding whitespace removed.
- `{attachments_details}`: one line per attachment, or nothing.
- `\n` (backslash, n) is a newline; a real newline in a template file works too.

The template is rendered in a single pass: text substituted for a placeholder
is never scanned again. The result has surrounding whitespace removed.

**Parse modes.** With `--telegram-api-parse-mode`, the template is Telegram
markup and the substituted values are escaped for that mode, so mail content
can never break the markup: e.g. with `MarkdownV2` and the template
`*{subject}*\n{body}`, the subject is bold and a `.` or `(` in it arrives
as typed.

**Body.** Character sets and transfer encodings are decoded (UTF-8, Latin-1,
CJK encodings, quoted-printable, base64, RFC 2047 headers). The body is the
text/plain part; a message with only HTML gets it converted to text. A
text/plain part marked as an attachment without a file name (what GNU mailx
sends) is used as the body when there is no other. Data that cannot be
parsed as a message at all is forwarded as it is. Line endings arrive as
`\n`.

**Attachments.** Every other part is an attachment and gets a line in
`{attachments_details}`:

```
Attachments:
- 🔗 inline.jpg (image/jpeg) 3B, sending...
- 📎 report.pdf (application/pdf) 2.048kB, sending...
- 📎 dump.tar (application/x-tar) 12.5MB, discarded
```

`🔗` is an inline part, `📎` a regular attachment. JPEG and PNG files up to
the photo limit go with `sendPhoto`, everything else up to the document limit
with `sendDocument`, larger ones are discarded. `application/octet-stream`
parts get a type guessed from the file name. Files are sent as silent
replies to the message, captioned with their file name.

**Long messages.** Telegram accepts at most 4096 characters, counted in
UTF-16 code units (an emoji counts two). A message longer than
`--message-length-to-send-as-file` keeps its template and has the body cut
and ended with `[truncated]`; the full text follows as `full_message.txt`
(plain text, unescaped). If even the template alone is too long, the text is
cut hard and sent without a parse mode, since the cut could split markup. If
the full text is larger than the document limit, the truncated message is
still sent, without the file.

## Delivery and errors

Delivery happens inside the SMTP transaction; there is no queue. The client
gets:

- `250` after the message reached every chat (attachment failures are logged
  and ignored, unless `--forwarded-attachment-respect-errors`);
- `451 4.3.0` with the reason (the bot token masked) when Telegram could not
  be reached or refused the message, so the sender keeps the mail and retries
  later. Chats are tried in order and the first failure stops the delivery, so
  a retry may repeat the message in chats that already got it;
- `552` for a message over the size limit.

Each message is logged with the client address, the authenticated user (or
anonymous), sender, recipients and size. Message content and secrets are not
logged.

## SMTP details

EHLO advertises `SIZE`, `8BITMIME`, `PIPELINING`, `ENHANCEDSTATUSCODES` and,
where allowed, `AUTH PLAIN LOGIN`. Supported commands: `HELO`, `EHLO`,
`AUTH`, `MAIL`, `RCPT`, `DATA`, `RSET`, `NOOP`, `VRFY` (always `252`), `HELP`,
`QUIT`. Any recipient domain is accepted. Limits: 100 recipients per message,
command lines up to 4096 bytes, five unrecognised commands per connection.
On `SIGTERM`, `SIGINT` or `SIGQUIT` the server stops accepting, tells idle
clients `421`, lets a delivery in progress finish, and exits (waiting at most
60 seconds).

## NixOS

The flake provides `packages.<system>.default` and `nixosModules.default`:

```nix
{
  inputs.smtp-to-telegram = {
    url = "github:ly4096x/smtp_to_telegram/rust";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  # In a NixOS configuration:
  imports = [ inputs.smtp-to-telegram.nixosModules.default ];

  services.smtp-to-telegram = {
    enable = true;
    listen = [ "0.0.0.0:25" ];
    # Accept both authenticated and anonymous mail.
    allowAnonymous = true;
    credentialsFile = config.age.secrets.smtp-to-telegram-users.path;
    # AUTH only over the VPN.
    plaintextAuthNetworks = [ "10.8.0.0/24" ];
    botTokenFile = config.age.secrets.telegram-bot-token.path;
    chatIdsFile = config.age.secrets.telegram-chat-ids.path;   # or: chatIds = [ "123456789" ];
    parseMode = "MarkdownV2";
    messageTemplate = "*{subject}*\\n{from} → {to}\\n\\n{body}\\n\\n{attachments_details}";
  };

  # Only the VPN interface may reach port 25.
  networking.firewall.interfaces."tun0".allowedTCPPorts = [ 25 ];
}
```

The secret files are handed over with `LoadCredential=` and never reach the
Nix store or the command line. The unit runs as a `DynamicUser` whose only
capability is `CAP_NET_BIND_SERVICE`, with `ProtectSystem=strict`, a system
call filter and the usual sandboxing; `systemd-analyze security` rates it
"OK" (the VM test checks the score stays at or below 2.0). Options not
covered by the module go in `extraArgs`.

## Migrating from the Go version

| Go | Rust |
| --- | --- |
| `ST_SMTP_LISTEN`, `ST_SMTP_PRIMARY_HOST`, `ST_SMTP_MAX_ENVELOPE_SIZE` | Same names. The fork had a fixed 10 MiB limit; the default is now 50 MB. |
| `ST_TELEGRAM_CHAT_IDS`, `ST_TELEGRAM_API_PREFIX`, `ST_TELEGRAM_API_TIMEOUT_SECONDS` | Same. |
| `ST_TELEGRAM_BOT_TOKEN` / `--telegram-bot-token` | The variable still works; the flag is gone, use `--telegram-bot-token-file`. |
| `ST_TELEGRAM_MESSAGE_TEMPLATE` | Same. The default now ends with `{attachments_details}` and the body is trimmed. |
| `ST_TELEGRAM_API_PARSE_MODE` / `--telegram-api-parsemode` | `ST_TELEGRAM_API_PARSE_MODE` / `--telegram-api-parse-mode`. Values are now escaped in HTML and Markdown mode too, not only MarkdownV2; a backslash is escaped in MarkdownV2. |
| `ST_TELEGRAM_API_POSTFIX` | Removed (the program refuses to start if it is set). Use the parse mode option and `--telegram-api-extra-param`. |
| Upstream's `ST_FORWARDED_ATTACHMENT_*`, `ST_MESSAGE_LENGTH_TO_SEND_AS_FILE`, `ST_LOG_LEVEL` | Same names; these features were not in the fork. |
| Anyone could send | Set `--allow-anonymous` to keep that, and/or configure credentials. |
| Telegram failure: `554` (the sender gives up) | `451` (the sender retries), as upstream later changed it. |

## Why a hand-written SMTP server

The Rust options were `samotop` (async-std, last release January 2022),
`mailin-embedded` (blocking I/O on a fixed thread pool) and `smtp-proto`
(maintained, but a parser rather than a server). The subset needed here is
small, so `src/smtp.rs` implements it directly on tokio: one runtime, no
blocking threads, bounded line lengths and timeouts, and the authentication
policy lives in code that the tests drive directly. MIME parsing uses
[`mail-parser`](https://crates.io/crates/mail-parser); the Bot API client is
[`reqwest`](https://crates.io/crates/reqwest) with rustls.

## Development

```sh
nix develop            # cargo, rustc, clippy, rustfmt, swaks
cargo test             # unit tests and end-to-end tests over real SMTP
cargo clippy --all-targets -- -D warnings
cargo fmt --check
nix flake check        # the package, clippy, rustfmt, and a NixOS VM test of the module
```

The end-to-end tests start the real server on an ephemeral port, send mail
with `lettre` or a raw SMTP client, and capture what reaches a mock Bot API.

## License

MIT, see [LICENSE](LICENSE).
