//! PIN gate for actions marked `pin = true` (schema section 14).
//!
//! `security { pin = <secret>; unlock_for = 1m; attempts = 5;
//! lockout = 5m }`. The secret holds an argon2 PHC hash, never the
//! PIN itself. A correct entry unlocks every PIN-gated action for
//! `unlock_for`; `attempts` wrong entries in a row lock the gate for
//! `lockout`. A `pin = true` action with no `security.pin` configured
//! fails closed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use paneld_config::parse::{AssignTarget, Document, Item};
use paneld_ui::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Ok,
    /// Wrong PIN; `remaining` tries before lockout.
    Wrong { remaining: u32 },
    /// This entry used up the last attempt, or the gate was already
    /// locked; `for_` is how long until it accepts entries again.
    LockedOut { for_: Duration },
    /// `pin = true` but no usable `security.pin` hash: fail closed.
    NotConfigured,
}

pub struct Gate {
    hash: Option<String>,
    unlock_for: Duration,
    max_attempts: u32,
    lockout: Duration,
    unlocked_until: Option<Instant>,
    locked_until: Option<Instant>,
    failures: u32,
}

impl Gate {
    pub fn from_doc(doc: &Document, secrets: &BTreeMap<String, Value>) -> Self {
        let mut g = Self {
            hash: None,
            unlock_for: Duration::from_secs(60),
            max_attempts: 5,
            lockout: Duration::from_secs(300),
            unlocked_until: None,
            locked_until: None,
            failures: 0,
        };
        let mut apply = |key: &str, raw: &str| {
            let raw = raw.trim();
            match key {
                "pin" => {
                    g.hash = match secrets.get(raw) {
                        Some(Value::String(s)) if !s.trim().is_empty() => {
                            Some(s.trim().to_string())
                        }
                        _ => {
                            eprintln!(
                                "paneld: security.pin secret `{raw}` is empty or unset; \
                                 PIN-gated actions will refuse to run"
                            );
                            None
                        }
                    }
                }
                "unlock_for" => {
                    if let Some(d) = paneld_config::value::parse_duration(raw) {
                        g.unlock_for = d;
                    }
                }
                "attempts" => {
                    if let Ok(n) = raw.parse::<u32>() {
                        g.max_attempts = n.max(1);
                    }
                }
                "lockout" => {
                    if let Some(d) = paneld_config::value::parse_duration(raw) {
                        g.lockout = d;
                    }
                }
                _ => {}
            }
        };
        for item in &doc.items {
            match item {
                Item::Assign(a) => {
                    if let AssignTarget::Qualified { category, key } = &a.target {
                        if category.text == "security" {
                            apply(&key.text, &a.value.text);
                        }
                    }
                }
                Item::Block(b) if b.kind.text == "security" => {
                    for inner in &b.items {
                        if let Item::Assign(a) = inner {
                            if let AssignTarget::Simple { key } = &a.target {
                                apply(&key.text, &a.value.text);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        g
    }

    /// Keep the runtime state (unlock window, lockout, failure count)
    /// of the gate this one replaces, so a config reload neither
    /// resets a lockout nor re-locks an unlocked panel.
    pub fn carry_from(&mut self, old: &Gate) {
        self.unlocked_until = old.unlocked_until;
        self.locked_until = old.locked_until;
        self.failures = old.failures;
    }

    pub fn is_unlocked(&self, now: Instant) -> bool {
        self.unlocked_until.is_some_and(|t| now < t)
    }

    /// Time left on an active lockout, if any.
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .and_then(|t| t.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    pub fn configured(&self) -> bool {
        self.hash.is_some()
    }

    /// Verify `digits` against the configured hash and update the
    /// unlock / lockout state. The digits are never logged.
    pub fn check(&mut self, digits: &str, now: Instant) -> Check {
        let Some(hash) = self.hash.as_deref() else {
            return Check::NotConfigured;
        };
        if let Some(d) = self.locked_for(now) {
            return Check::LockedOut { for_: d };
        }
        let parsed = match PasswordHash::new(hash) {
            Ok(p) => p,
            Err(_) => {
                eprintln!("paneld: security.pin is not a valid argon2 PHC hash");
                return Check::NotConfigured;
            }
        };
        if Argon2::default()
            .verify_password(digits.as_bytes(), &parsed)
            .is_ok()
        {
            self.failures = 0;
            self.locked_until = None;
            self.unlocked_until = Some(now + self.unlock_for);
            return Check::Ok;
        }
        self.failures += 1;
        if self.failures >= self.max_attempts {
            self.failures = 0;
            self.locked_until = Some(now + self.lockout);
            return Check::LockedOut { for_: self.lockout };
        }
        Check::Wrong {
            remaining: self.max_attempts - self.failures,
        }
    }
}

/// Hash a PIN into an argon2id PHC string for `security.pin`. Salt
/// comes from the kernel CSPRNG.
pub fn hash(pin: &str) -> Result<String, String> {
    let mut salt = [0u8; 16];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut salt))
            .map_err(|e| format!("reading /dev/urandom: {e}"))?;
    }
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(hash: &str, attempts: u32) -> Gate {
        Gate {
            hash: Some(hash.to_string()),
            unlock_for: Duration::from_secs(60),
            max_attempts: attempts,
            lockout: Duration::from_secs(300),
            unlocked_until: None,
            locked_until: None,
            failures: 0,
        }
    }

    #[test]
    fn correct_pin_unlocks_for_the_window() {
        let h = hash("1234").unwrap();
        assert!(h.starts_with("$argon2id$"));
        let mut g = gate(&h, 5);
        let t0 = Instant::now();
        assert!(!g.is_unlocked(t0));
        assert_eq!(g.check("1234", t0), Check::Ok);
        assert!(g.is_unlocked(t0 + Duration::from_secs(59)));
        assert!(!g.is_unlocked(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn wrong_pins_count_down_then_lock_out() {
        let h = hash("1234").unwrap();
        let mut g = gate(&h, 3);
        let t0 = Instant::now();
        assert_eq!(g.check("0000", t0), Check::Wrong { remaining: 2 });
        assert_eq!(g.check("1111", t0), Check::Wrong { remaining: 1 });
        assert_eq!(
            g.check("2222", t0),
            Check::LockedOut { for_: Duration::from_secs(300) }
        );
        // Even the right PIN is refused while locked.
        assert!(matches!(
            g.check("1234", t0 + Duration::from_secs(10)),
            Check::LockedOut { .. }
        ));
        // After the lockout it works again.
        assert_eq!(g.check("1234", t0 + Duration::from_secs(301)), Check::Ok);
    }

    #[test]
    fn success_resets_the_failure_count() {
        let h = hash("1234").unwrap();
        let mut g = gate(&h, 3);
        let t0 = Instant::now();
        assert_eq!(g.check("0000", t0), Check::Wrong { remaining: 2 });
        assert_eq!(g.check("1234", t0), Check::Ok);
        assert_eq!(g.check("0000", t0), Check::Wrong { remaining: 2 });
    }

    #[test]
    fn missing_or_bad_hash_fails_closed() {
        let mut g = gate("not-a-phc-string", 5);
        assert_eq!(g.check("1234", Instant::now()), Check::NotConfigured);
        let mut g = Gate { hash: None, ..gate("x", 5) };
        assert_eq!(g.check("1234", Instant::now()), Check::NotConfigured);
    }

    #[test]
    fn from_doc_reads_security_block_and_secret() {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let src = "secret pin_hash { env = X }\n\
                   security { pin = pin_hash; unlock_for = 2m; attempts = 3; lockout = 10m }\n";
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        let mut secrets = BTreeMap::new();
        secrets.insert("pin_hash".to_string(), Value::String("$argon2id$x".into()));
        let g = Gate::from_doc(&doc, &secrets);
        assert!(g.configured());
        assert_eq!(g.unlock_for, Duration::from_secs(120));
        assert_eq!(g.max_attempts, 3);
        assert_eq!(g.lockout, Duration::from_secs(600));
    }
}
