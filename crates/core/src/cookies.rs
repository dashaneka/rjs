//! Login cookies, from a `cookies.txt` file or straight from a browser's cookie store.
//! Only cookies for the supported sites are kept.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct Cookie {
    pub domain: String,
    /// Also sent to subdomains (`.instagram.com` style cookies).
    pub include_subdomains: bool,
    pub path: String,
    pub secure: bool,
    pub name: String,
    pub value: String,
}

const SITES: [&str; 6] = [
    "instagram.com",
    "tiktok.com",
    "x.com",
    "twitter.com",
    "facebook.com",
    "fb.watch",
];

pub(crate) fn for_supported_site(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    SITES
        .iter()
        .any(|site| domain == *site || domain.ends_with(&format!(".{site}")))
}

fn fail(message: impl Into<String>) -> Error {
    Error::Cookies(message.into())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Reads a Netscape-format `cookies.txt`, as exported by browser extensions.
pub fn from_file(path: &Path) -> Result<Vec<Cookie>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| fail(format!("couldn't read {}: {e}", path.display())))?;
    let cookies = parse_cookies_txt(&text);
    if cookies.is_empty()
        && text
            .lines()
            .any(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        return Err(fail(format!(
            "{} isn't a cookies.txt file (Netscape format)",
            path.display()
        )));
    }
    Ok(cookies
        .into_iter()
        .filter(|c| for_supported_site(&c.domain))
        .collect())
}

/// Every valid, unexpired cookie in the file, for any site.
fn parse_cookies_txt(text: &str) -> Vec<Cookie> {
    let now = now();
    text.lines()
        .map(|line| line.trim_end_matches('\r'))
        .map(|line| line.strip_prefix("#HttpOnly_").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [domain, subdomains, path, secure, expires, name, value] = fields[..] else {
                return None;
            };
            let expires: i64 = expires.parse().unwrap_or(0);
            if expires != 0 && expires < now {
                return None;
            }
            Some(Cookie {
                domain: domain.to_owned(),
                include_subdomains: subdomains.eq_ignore_ascii_case("TRUE")
                    || domain.starts_with('.'),
                path: path.to_owned(),
                secure: secure.eq_ignore_ascii_case("TRUE"),
                name: name.to_owned(),
                value: value.to_owned(),
            })
        })
        .collect()
}

/// Reads cookies from an installed browser. `spec` is a browser name, optionally
/// followed by a profile: `chrome`, `chrome:Profile 1`, `firefox`.
pub fn from_browser(spec: &str) -> Result<Vec<Cookie>> {
    let (name, profile) = match spec.split_once(':') {
        Some((name, profile)) => (name.trim().to_ascii_lowercase(), Some(profile.trim())),
        None => (spec.trim().to_ascii_lowercase(), None),
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| fail("couldn't find your home folder"))?;

    match name.as_str() {
        "firefox" => firefox::cookies(&home.join(".mozilla/firefox"), profile),
        _ => {
            let (dir, keyring) = match name.as_str() {
                "chrome" | "google-chrome" => (".config/google-chrome", "chrome"),
                "chromium" => (".config/chromium", "chromium"),
                "brave" => (".config/BraveSoftware/Brave-Browser", "brave"),
                "edge" | "microsoft-edge" => (".config/microsoft-edge", "microsoft-edge"),
                other => {
                    return Err(fail(format!(
                        "unknown browser \"{other}\"; use chrome, chromium, brave, edge or firefox"
                    )));
                }
            };
            chromium::cookies(&home.join(dir), profile.unwrap_or("Default"), keyring)
        }
    }
}

/// Opens a copy of a browser's cookie database, since the browser keeps the original locked.
fn open_copy(db: &Path) -> Result<(rusqlite::Connection, TempCopy)> {
    let copy =
        TempCopy(std::env::temp_dir().join(format!("rjs-cookies-{}.sqlite", std::process::id())));
    std::fs::copy(db, &copy.0).map_err(|e| fail(format!("couldn't read {}: {e}", db.display())))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            let _ = std::fs::copy(&side, format!("{}{suffix}", copy.0.display()));
        }
    }
    let conn = rusqlite::Connection::open(&copy.0)
        .map_err(|e| fail(format!("couldn't open the cookie database: {e}")))?;
    Ok((conn, copy))
}

struct TempCopy(PathBuf);

impl Drop for TempCopy {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn sql_error(e: rusqlite::Error) -> Error {
    fail(format!("couldn't read the cookie database: {e}"))
}

/// Chrome, Chromium, Brave and Edge, which share one storage format.
mod chromium {
    use std::path::Path;
    use std::process::Command;

    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::{BlockModeDecrypt, KeyIvInit};

    use super::{Cookie, fail, for_supported_site, now, open_copy, sql_error};
    use crate::Result;

    type Decryptor = cbc::Decryptor<aes::Aes128>;

    /// Microseconds between 1601-01-01 (Chrome's epoch) and 1970-01-01.
    const EPOCH_OFFSET_SECS: i64 = 11_644_473_600;
    /// From this database version on, decrypted values start with a 32-byte hash of the domain.
    const HASH_PREFIX_SINCE: i64 = 24;

    pub(super) fn cookies(base: &Path, profile: &str, keyring: &str) -> Result<Vec<Cookie>> {
        if !base.exists() {
            return Err(fail(format!(
                "{} isn't installed (no {})",
                keyring,
                base.display()
            )));
        }
        let dir = base.join(profile);
        let db = ["Network/Cookies", "Cookies"]
            .iter()
            .map(|p| dir.join(p))
            .find(|p| p.exists())
            .ok_or_else(|| {
                fail(format!(
                    "no cookies found for profile \"{profile}\" in {}",
                    base.display()
                ))
            })?;

        let (conn, _copy) = open_copy(&db)?;
        let version: i64 = conn
            .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut stmt = conn
            .prepare("SELECT host_key, path, is_secure, expires_utc, name, value, encrypted_value FROM cookies")
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                ))
            })
            .map_err(sql_error)?;

        let mut keys = Keys::new(keyring);
        let (mut cookies, mut undecryptable) = (Vec::new(), 0);
        let now = now();
        for row in rows {
            let (host, path, secure, expires, name, value, encrypted) = row.map_err(sql_error)?;
            if !for_supported_site(&host) {
                continue;
            }
            if expires != 0 && expires / 1_000_000 - EPOCH_OFFSET_SECS < now {
                continue;
            }
            let value = if !value.is_empty() {
                value
            } else {
                match keys.decrypt(&encrypted, version >= HASH_PREFIX_SINCE) {
                    Some(value) => value,
                    None => {
                        undecryptable += 1;
                        continue;
                    }
                }
            };
            cookies.push(Cookie {
                include_subdomains: host.starts_with('.'),
                domain: host,
                path,
                secure: secure != 0,
                name,
                value,
            });
        }

        if cookies.is_empty() && undecryptable > 0 {
            return Err(fail("couldn't decrypt the browser's cookies; make sure you're logged in to your desktop \
                 session (the keyring must be unlocked) and that `secret-tool` is installed".to_string()));
        }
        Ok(cookies)
    }

    /// Cookie encryption keys: `v10` uses a fixed password, `v11` one stored in the
    /// desktop keyring, which is only looked up when needed.
    struct Keys {
        keyring: String,
        v10: [u8; 16],
        v11: Option<Option<[u8; 16]>>,
    }

    impl Keys {
        fn new(keyring: &str) -> Self {
            Self {
                keyring: keyring.to_owned(),
                v10: derive(b"peanuts"),
                v11: None,
            }
        }

        fn decrypt(&mut self, encrypted: &[u8], hash_prefix: bool) -> Option<String> {
            if encrypted.len() < 3 {
                return None;
            }
            let (version, data) = encrypted.split_at(3);
            let key = match version {
                b"v10" => self.v10,
                b"v11" => {
                    let keyring = &self.keyring;
                    (*self
                        .v11
                        .get_or_insert_with(|| keyring_password(keyring).map(|p| derive(&p))))?
                }
                _ => return None,
            };

            let mut buf = data.to_vec();
            let plain = Decryptor::new(&key.into(), &[b' '; 16].into())
                .decrypt_padded::<Pkcs7>(&mut buf)
                .ok()?;
            let plain = match hash_prefix {
                true if plain.len() >= 32 => &plain[32..],
                _ => plain,
            };
            String::from_utf8(plain.to_vec()).ok()
        }
    }

    fn derive(password: &[u8]) -> [u8; 16] {
        let mut key = [0; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
        key
    }

    /// Asks the desktop keyring (GNOME Keyring / KWallet via Secret Service) for the browser's key.
    fn keyring_password(application: &str) -> Option<Vec<u8>> {
        let output = Command::new("secret-tool")
            .args(["lookup", "application", application])
            .output()
            .ok()?;
        let password = output.stdout.trim_ascii();
        (output.status.success() && !password.is_empty()).then(|| password.to_vec())
    }

    #[cfg(test)]
    mod tests {
        use aes::cipher::BlockModeEncrypt;

        use super::*;

        fn encrypt(key: [u8; 16], plain: &[u8]) -> Vec<u8> {
            let mut buf = plain.to_vec();
            buf.resize(plain.len() + 16, 0);
            cbc::Encryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
                .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
                .unwrap()
                .to_vec()
        }

        #[test]
        fn decrypts_v10_values() {
            let mut keys = Keys::new("test");
            let mut encrypted = b"v10".to_vec();
            encrypted.extend(encrypt(derive(b"peanuts"), b"abc123"));
            assert_eq!(keys.decrypt(&encrypted, false).as_deref(), Some("abc123"));
        }

        #[test]
        fn strips_domain_hash_in_new_databases() {
            let mut keys = Keys::new("test");
            let mut plain = vec![7u8; 32];
            plain.extend(b"session");
            let mut encrypted = b"v10".to_vec();
            encrypted.extend(encrypt(derive(b"peanuts"), &plain));
            assert_eq!(keys.decrypt(&encrypted, true).as_deref(), Some("session"));
        }
    }
}

mod firefox {
    use std::path::{Path, PathBuf};

    use super::{Cookie, fail, for_supported_site, now, open_copy, sql_error};
    use crate::Result;

    pub(super) fn cookies(base: &Path, profile: Option<&str>) -> Result<Vec<Cookie>> {
        let db = find_profile(base, profile)?;
        let (conn, _copy) = open_copy(&db)?;
        let mut stmt = conn
            .prepare("SELECT host, path, isSecure, expiry, name, value FROM moz_cookies")
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(sql_error)?;

        let now = now();
        let mut cookies = Vec::new();
        for row in rows {
            let (host, path, secure, expiry, name, value) = row.map_err(sql_error)?;
            // Newer Firefox versions store milliseconds.
            let expiry = if expiry > 100_000_000_000 {
                expiry / 1000
            } else {
                expiry
            };
            if !for_supported_site(&host) || (expiry != 0 && expiry < now) {
                continue;
            }
            cookies.push(Cookie {
                include_subdomains: host.starts_with('.'),
                domain: host,
                path,
                secure: secure != 0,
                name,
                value,
            });
        }
        Ok(cookies)
    }

    /// The named profile, or else the one used most recently.
    fn find_profile(base: &Path, wanted: Option<&str>) -> Result<PathBuf> {
        let entries = std::fs::read_dir(base)
            .map_err(|_| fail("Firefox isn't installed (no ~/.mozilla/firefox)"))?;
        let mut profiles: Vec<(PathBuf, std::time::SystemTime)> = entries
            .flatten()
            .map(|e| e.path().join("cookies.sqlite"))
            .filter_map(|db| Some((db.clone(), db.metadata().ok()?.modified().ok()?)))
            .collect();
        if let Some(wanted) = wanted {
            profiles.retain(|(db, _)| {
                db.parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == wanted || n.ends_with(&format!(".{wanted}")))
            });
        }
        profiles
            .into_iter()
            .max_by_key(|(_, modified)| *modified)
            .map(|(db, _)| db)
            .ok_or_else(|| fail("no Firefox profile with cookies found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cookies_txt_for_supported_sites_only() {
        let text = "# Netscape HTTP Cookie File\n\
            .instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tabc\n\
            #HttpOnly_.tiktok.com\tTRUE\t/\tTRUE\t0\tsid_tt\tdef\n\
            .example.com\tTRUE\t/\tFALSE\t0\tother\tx\n\
            .x.com\tTRUE\t/\tTRUE\t1\texpired\ty\n";
        let path = std::env::temp_dir().join(format!("rjs-test-{}.txt", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let cookies = from_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["sessionid", "sid_tt"]);
        assert!(cookies[0].include_subdomains && cookies[0].secure);
    }

    #[test]
    fn file_with_only_other_sites_is_still_valid() {
        let cookies = parse_cookies_txt(".example.com\tTRUE\t/\tFALSE\t0\tx\ty\n");
        assert_eq!(cookies.len(), 1);
    }

    #[test]
    fn matches_site_domains() {
        assert!(for_supported_site(".instagram.com"));
        assert!(for_supported_site("www.tiktok.com"));
        assert!(!for_supported_site("notx.com"));
        assert!(!for_supported_site(".google.com"));
    }
}
