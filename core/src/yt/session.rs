//! A signed-in YouTube Music session: the account's cookies, stored encrypted with Windows DPAPI
//! (readable only by this Windows user), and the `SAPISIDHASH` authorization derived from them.

use crate::paths::{config_dir, unix_now, write_atomic};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

pub const ORIGIN: &str = "https://music.youtube.com";

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    /// name → value, for `.youtube.com` / `music.youtube.com` / `.google.com` as captured.
    pub cookies: Vec<(String, String)>,
    /// Unix seconds when signed in.
    pub created: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print cookie values.
        write!(f, "Session({} cookies)", self.cookies.len())
    }
}

impl Session {
    pub fn new(cookies: Vec<(String, String)>) -> Result<Self> {
        let s = Session {
            cookies,
            created: unix_now(),
        };
        if s.sapisid().is_none() {
            return Err(Error::Unavailable(
                "not signed in (no SAPISID cookie)".into(),
            ));
        }
        Ok(s)
    }

    /// From a raw `Cookie:` header copied out of a browser (fallback sign-in).
    pub fn from_cookie_header(header: &str) -> Result<Self> {
        let header = header
            .trim()
            .trim_start_matches("Cookie:")
            .trim_start_matches("cookie:");
        let cookies = header
            .split(';')
            .filter_map(|kv| {
                let (k, v) = kv.trim().split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        Self::new(cookies)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn sapisid(&self) -> Option<&str> {
        self.get("__Secure-3PAPISID")
            .or_else(|| self.get("SAPISID"))
    }

    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// `SAPISIDHASH <ts>_<sha1(ts + " " + SAPISID + " " + origin)>`.
    pub fn authorization(&self) -> String {
        let ts = unix_now();
        let sapisid = self.sapisid().unwrap_or_default();
        let digest = Sha1::digest(format!("{ts} {sapisid} {ORIGIN}").as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("SAPISIDHASH {ts}_{hex}")
    }

    fn path() -> std::path::PathBuf {
        config_dir().join("session.bin")
    }

    /// The stored session, if there is one and it decrypts.
    pub fn load() -> Option<Session> {
        let blob = std::fs::read(Self::path()).ok()?;
        let plain = dpapi::unprotect(&blob).ok()?;
        serde_json::from_slice(&plain).ok()
    }

    pub fn save(&self) -> Result<()> {
        let plain = serde_json::to_vec(self).expect("json");
        let blob = dpapi::protect(&plain)?;
        write_atomic(&Self::path(), &blob).map_err(|e| Error::Unavailable(e.to_string()))
    }

    /// Sign out: forget the stored session.
    pub fn delete() {
        let _ = std::fs::remove_file(Self::path());
    }
}

mod dpapi {
    use crate::{Error, Result};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn run(data: &[u8], protect: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input points at `data` for the duration of the call; the output buffer is
        // allocated by Windows and freed with LocalFree below.
        unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    None,
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    None,
                    None,
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
            .map_err(|e| Error::Unavailable(format!("DPAPI: {e}")))?;
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(v)
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        run(data, true)
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        run(data, false)
    }

    #[cfg(test)]
    #[test]
    fn round_trip() {
        let blob = protect(b"secret").unwrap();
        assert_ne!(&blob[..], b"secret");
        assert_eq!(unprotect(&blob).unwrap(), b"secret");
    }
}
