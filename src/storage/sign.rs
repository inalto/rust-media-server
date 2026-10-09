//! Signed playback URLs of private encodings (docs/ACCOUNTS.md §5).
//!
//! `token = base64url(HMAC-SHA256(key, "<vod_id>|<expires>"))` without padding, where
//! `key` is the bucket's `signing_key` **as text** (its 64 hex characters, so a client
//! can compute tokens with `openssl dgst -sha256 -hmac "$KEY"`) and `expires` the Unix
//! time (seconds) after which the token is refused. The token travels as the query
//! `?token=<t>&expires=<unix>` or in the cookie `rms_play_<vod_id>=<t>:<expires>`.
//! Verification decodes the token and compares the MAC in constant time
//! (`Mac::verify_slice`). Rotating the bucket key invalidates every token.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Default lifetime of a signed URL.
pub const DEFAULT_EXPIRES_SECS: u64 = 3600;
/// Longest lifetime accepted by the signing endpoints (one year).
pub const MAX_EXPIRES_SECS: u64 = 365 * 24 * 3600;

fn mac(signing_key: &str, vod_id: &str, expires: i64) -> HmacSha256 {
    // HMAC accepts keys of any length: `new_from_slice` cannot fail.
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(signing_key.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
    m.update(vod_id.as_bytes());
    m.update(b"|");
    m.update(expires.to_string().as_bytes());
    m
}

/// The token of `vod_id` valid until `expires` (Unix seconds).
pub fn token(signing_key: &str, vod_id: &str, expires: i64) -> String {
    URL_SAFE_NO_PAD.encode(mac(signing_key, vod_id, expires).finalize().into_bytes())
}

/// True when `token` is the token of `vod_id`/`expires` and `expires` is after `now`.
pub fn verify(signing_key: &str, vod_id: &str, token: &str, expires: i64, now: i64) -> bool {
    if expires <= now || signing_key.is_empty() {
        return false;
    }
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(token.trim_end_matches('=')) else {
        return false;
    };
    mac(signing_key, vod_id, expires)
        .verify_slice(&bytes)
        .is_ok()
}

/// Name of the playback cookie of a video.
pub fn cookie_name(vod_id: &str) -> String {
    format!("rms_play_{vod_id}")
}

/// A token with its expiry, as found in a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedToken {
    pub token: String,
    pub expires: i64,
}

impl SignedToken {
    /// `token=<t>&expires=<unix>` (both URL-safe).
    pub fn query(&self) -> String {
        format!("token={}&expires={}", self.token, self.expires)
    }

    /// Cookie value `<t>:<expires>`.
    pub fn cookie_value(&self) -> String {
        format!("{}:{}", self.token, self.expires)
    }

    pub fn from_cookie_value(v: &str) -> Option<Self> {
        let (token, expires) = v.trim().rsplit_once(':')?;
        Some(Self {
            token: token.to_string(),
            expires: expires.parse().ok()?,
        })
    }

    /// `token` and `expires` of a query string (`None` unless both are present).
    pub fn from_query(query: Option<&str>) -> Option<Self> {
        let (mut token, mut expires) = (None, None);
        for pair in query?.split('&') {
            match pair.split_once('=') {
                Some(("token", v)) if token.is_none() && !v.is_empty() => token = Some(v),
                Some(("expires", v)) if expires.is_none() => expires = v.parse::<i64>().ok(),
                _ => {}
            }
        }
        Some(Self {
            token: token?.to_string(),
            expires: expires?,
        })
    }

    /// A new token of `vod_id` valid for `secs` from now.
    pub fn issue(signing_key: &str, vod_id: &str, secs: u64) -> Self {
        let expires = chrono::Utc::now().timestamp() + secs.min(MAX_EXPIRES_SECS) as i64;
        Self {
            token: token(signing_key, vod_id, expires),
            expires,
        }
    }
}

/// `url` with `query` appended (`?` or `&`).
pub fn append_query(url: &str, query: &str) -> String {
    if query.is_empty() {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&{query}")
    } else {
        format!("{url}?{query}")
    }
}

/// HLS playlist with `query` appended to every URI (`URI="…"` attributes and URI lines):
/// players that do not forward the page query (native HLS) still send the token.
pub fn rewrite_hls(text: &str, query: &str) -> String {
    let mut out = String::with_capacity(text.len() + 128);
    for line in text.lines() {
        if line.starts_with('#') {
            let mut rest = line;
            while let Some(i) = rest.find("URI=\"") {
                let start = i + 5;
                out.push_str(&rest[..start]);
                rest = &rest[start..];
                let Some(end) = rest.find('"') else { break };
                out.push_str(&append_query(&rest[..end], query));
                rest = &rest[end..];
            }
            out.push_str(rest);
        } else if !line.trim().is_empty() {
            out.push_str(&append_query(line.trim(), query));
        }
        out.push('\n');
    }
    out
}

/// MPD with `query` appended to the `initialization`/`media` templates (XML-escaped).
pub fn rewrite_mpd(text: &str, query: &str) -> String {
    let escaped = query.replace('&', "&amp;");
    let mut out = text.to_string();
    for key in ["initialization=\"", "media=\""] {
        let mut result = String::with_capacity(out.len() + 128);
        let mut rest = out.as_str();
        while let Some(i) = rest.find(key) {
            let boundary = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
            let start = i + key.len();
            result.push_str(&rest[..start]);
            rest = &rest[start..];
            let Some(end) = rest.find('"') else { break };
            let value = &rest[..end];
            if boundary {
                let sep = if value.contains('?') { "&amp;" } else { "?" };
                result.push_str(&format!("{value}{sep}{escaped}"));
            } else {
                result.push_str(value);
            }
            rest = &rest[end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn tokens() {
        let t = token(KEY, "vid", 2_000_000_000);
        assert_eq!(t.len(), 43, "32 bytes, base64url without padding");
        assert!(!t.contains(['+', '/', '=']));
        assert!(verify(KEY, "vid", &t, 2_000_000_000, 1_000));
        assert!(
            !verify(KEY, "vid", &t, 2_000_000_000, 2_000_000_000),
            "expired"
        );
        assert!(
            !verify(KEY, "other", &t, 2_000_000_000, 1_000),
            "other video"
        );
        assert!(
            !verify(KEY, "vid", &t, 2_000_000_001, 1_000),
            "other expiry"
        );
        assert!(
            !verify("rotated", "vid", &t, 2_000_000_000, 1_000),
            "rotated key"
        );
        assert!(!verify(KEY, "vid", "garbage!", 2_000_000_000, 1_000));
        assert!(!verify(KEY, "vid", "", 2_000_000_000, 1_000));
        assert!(!verify("", "vid", &t, 2_000_000_000, 1_000));
        let mut tampered = t.clone().into_bytes();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(!verify(KEY, "vid", &tampered, 2_000_000_000, 1_000));
        // Interoperability: HMAC-SHA256 of "vid|2000000000" with the hex text as key.
        // printf 'vid|2000000000' | openssl dgst -sha256 -hmac "$KEY" -binary | base64 | tr '+/' '-_' | tr -d =
        let expected = {
            let mut m = <HmacSha256 as KeyInit>::new_from_slice(KEY.as_bytes()).unwrap();
            m.update(b"vid|2000000000");
            URL_SAFE_NO_PAD.encode(m.finalize().into_bytes())
        };
        assert_eq!(t, expected);
    }

    #[test]
    fn query_and_cookie() {
        let s = SignedToken::issue(KEY, "vid", 60);
        assert!(verify(
            KEY,
            "vid",
            &s.token,
            s.expires,
            chrono::Utc::now().timestamp()
        ));
        let q = s.query();
        assert_eq!(SignedToken::from_query(Some(&q)), Some(s.clone()));
        assert_eq!(
            SignedToken::from_query(Some(&format!("v=1&{q}&token=zzz"))),
            Some(s.clone()),
            "first value wins"
        );
        assert_eq!(SignedToken::from_query(Some("token=a")), None);
        assert_eq!(SignedToken::from_query(Some("token=a&expires=x")), None);
        assert_eq!(SignedToken::from_query(None), None);
        assert_eq!(
            SignedToken::from_cookie_value(&s.cookie_value()),
            Some(s.clone())
        );
        assert_eq!(SignedToken::from_cookie_value("nope"), None);
        assert_eq!(cookie_name("v1"), "rms_play_v1");
        assert!(
            SignedToken::issue(KEY, "v", u64::MAX).expires
                <= chrono::Utc::now().timestamp() + MAX_EXPIRES_SECS as i64
        );
        assert_eq!(append_query("a.m3u8", "t=1"), "a.m3u8?t=1");
        assert_eq!(append_query("a.m3u8?v=2", "t=1"), "a.m3u8?v=2&t=1");
        assert_eq!(append_query("a", ""), "a");
    }

    #[test]
    fn playlist_rewrites() {
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",URI=\"media_2.m3u8?v=r1\"\n\
                      #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"aud\"\nmedia_0.m3u8?v=r1\n";
        let out = rewrite_hls(master, "token=T&expires=9");
        assert!(
            out.contains("URI=\"media_2.m3u8?v=r1&token=T&expires=9\""),
            "{out}"
        );
        assert!(
            out.contains("\nmedia_0.m3u8?v=r1&token=T&expires=9\n"),
            "{out}"
        );
        assert!(out.contains("AUDIO=\"aud\"\n"), "{out}");
        let media = "#EXTM3U\n#EXT-X-MAP:URI=\"init-0.m4s?v=r1\"\n#EXTINF:4.0,\nchunk-0-00001.m4s?v=r1\n#EXT-X-ENDLIST\n";
        let out = rewrite_hls(media, "token=T&expires=9");
        assert!(out.contains("#EXT-X-MAP:URI=\"init-0.m4s?v=r1&token=T&expires=9\""));
        assert!(out.contains("\nchunk-0-00001.m4s?v=r1&token=T&expires=9\n"));
        assert!(out.ends_with("#EXT-X-ENDLIST\n"));
        let mpd = r#"<SegmentTemplate initialization="init-$RepresentationID$.m4s?v=r1" media="chunk-$RepresentationID$-$Number%05d$.m4s" xmedia="x"/>"#;
        let out = rewrite_mpd(mpd, "token=T&expires=9");
        assert!(
            out.contains(
                r#"initialization="init-$RepresentationID$.m4s?v=r1&amp;token=T&amp;expires=9""#
            ),
            "{out}"
        );
        assert!(
            out.contains(
                r#"media="chunk-$RepresentationID$-$Number%05d$.m4s?token=T&amp;expires=9""#
            ),
            "{out}"
        );
        assert!(out.contains(r#"xmedia="x""#));
    }
}
