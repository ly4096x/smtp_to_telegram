//! SMTP AUTH support: the credential store, the network rule that decides
//! where plaintext AUTH is offered, and SASL PLAIN decoding.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;

use ipnet::IpNet;

/// Username/password pairs accepted by `AUTH PLAIN` and `AUTH LOGIN`.
///
/// The text format (credentials file or `ST_SMTP_CREDENTIALS`) is one
/// `username:password` pair per line. The password is everything after the
/// first `:` and is taken verbatim (spaces included); only the line ending is
/// stripped. Empty lines and lines starting with `#` are ignored.
#[derive(Clone, Default)]
pub struct Credentials {
    users: HashMap<String, String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print passwords, not even in debug output.
        let mut users: Vec<&String> = self.users.keys().collect();
        users.sort();
        f.debug_struct("Credentials")
            .field("users", &users)
            .finish()
    }
}

impl Credentials {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut users = HashMap::new();
        for (index, raw_line) in text.split('\n').enumerate() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let line_no = index + 1;
            // Error messages carry line numbers only, never line contents:
            // the line holds a password.
            let (user, password) = line
                .split_once(':')
                .ok_or_else(|| format!("line {line_no}: expected `username:password`"))?;
            if user.is_empty() {
                return Err(format!("line {line_no}: empty username"));
            }
            if password.is_empty() {
                return Err(format!("line {line_no}: empty password"));
            }
            if users
                .insert(user.to_string(), password.to_string())
                .is_some()
            {
                return Err(format!("line {line_no}: duplicate username"));
            }
        }
        Ok(Self { users })
    }

    /// Builds a store from `(username, password)` pairs.
    pub fn from_pairs<I, U, P>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (U, P)>,
        U: Into<String>,
        P: Into<String>,
    {
        Self {
            users: pairs
                .into_iter()
                .map(|(u, p)| (u.into(), p.into()))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// Checks a username/password pair. The password comparison does not
    /// short-circuit, and an unknown user still costs one comparison.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        match self.users.get(user) {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => {
                let _ = constant_time_eq(password.as_bytes(), password.as_bytes());
                false
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// True when `ip` falls in one of `networks`. IPv4-mapped IPv6 addresses
/// (`::ffff:a.b.c.d`, seen on dual-stack `[::]` listeners) are matched as
/// IPv4.
pub fn ip_in_networks(ip: IpAddr, networks: &[IpNet]) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    networks.iter().any(|net| net.contains(&ip))
}

/// Decodes a SASL PLAIN message (`authzid NUL authcid NUL passwd`).
///
/// An authorization identity is accepted only when it is empty or equal to
/// the authentication identity: this server has no notion of acting on
/// behalf of another user.
pub fn decode_plain(decoded: &[u8]) -> Option<(String, String)> {
    let mut fields = decoded.split(|&b| b == 0);
    let authzid = fields.next()?;
    let authcid = fields.next()?;
    let password = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let authcid = std::str::from_utf8(authcid).ok()?;
    let authzid = std::str::from_utf8(authzid).ok()?;
    let password = std::str::from_utf8(password).ok()?;
    if !authzid.is_empty() && authzid != authcid {
        return None;
    }
    Some((authcid.to_string(), password.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credentials_file() {
        let creds =
            Credentials::parse("# comment\nalice:secret\r\n\nbob:pa:ss word \n  \n").unwrap();
        assert_eq!(creds.len(), 2);
        assert!(creds.verify("alice", "secret"));
        assert!(creds.verify("bob", "pa:ss word "));
        assert!(!creds.verify("bob", "pa:ss word"));
        assert!(!creds.verify("alice", "Secret"));
        assert!(!creds.verify("carol", "secret"));
    }

    #[test]
    fn rejects_bad_credentials_without_leaking_them() {
        let err = Credentials::parse("alice-secret\n").unwrap_err();
        assert!(err.contains("line 1"));
        assert!(!err.contains("secret"));
        assert!(Credentials::parse(":pw").is_err());
        assert!(Credentials::parse("user:").is_err());
        assert!(Credentials::parse("a:1\na:2").is_err());
    }

    #[test]
    fn debug_does_not_print_passwords() {
        let creds = Credentials::parse("alice:hunter2").unwrap();
        let printed = format!("{creds:?}");
        assert!(printed.contains("alice"));
        assert!(!printed.contains("hunter2"));
    }

    #[test]
    fn matches_networks_including_mapped_v4() {
        let nets: Vec<IpNet> = vec!["10.145.0.0/24".parse().unwrap(), "::1/128".parse().unwrap()];
        assert!(ip_in_networks("10.145.0.7".parse().unwrap(), &nets));
        assert!(ip_in_networks("::ffff:10.145.0.7".parse().unwrap(), &nets));
        assert!(ip_in_networks("::1".parse().unwrap(), &nets));
        assert!(!ip_in_networks("10.146.0.7".parse().unwrap(), &nets));
        assert!(!ip_in_networks("127.0.0.1".parse().unwrap(), &nets));
    }

    #[test]
    fn decodes_sasl_plain() {
        assert_eq!(
            decode_plain(b"\0alice\0secret"),
            Some(("alice".into(), "secret".into()))
        );
        assert_eq!(
            decode_plain(b"alice\0alice\0secret"),
            Some(("alice".into(), "secret".into()))
        );
        assert_eq!(decode_plain(b"bob\0alice\0secret"), None);
        assert_eq!(decode_plain(b"alice\0secret"), None);
        assert_eq!(decode_plain(b"\0a\0b\0c"), None);
    }
}
