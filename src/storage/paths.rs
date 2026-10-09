//! Paths of the storage API (docs/ACCOUNTS.md §3 "Rules").
//!
//! A request path below `/storage/{bucket}` is split on `/`, every segment is
//! percent-decoded (strictly: a malformed escape is an error, `+` stays `+`), must be
//! UTF-8 and is normalized to Unicode NFC. Rejected: empty segments (`a//b`), `.` and
//! `..`, control characters (Unicode `Cc`), `\`, a `/` produced by decoding (`%2F`),
//! names starting with `.rms` (reserved, any case), segments over 255 bytes and paths
//! over 1024 bytes. A trailing `/` names a directory; the empty path is the root.
//!
//! Normalized paths start with `/` and have no trailing `/` (`/videos/a.mp4`); the root
//! is `/`. Permission prefixes are compared against [`ObjectPath::access_path`]: the path
//! of a file, or the directory path **with** its trailing `/` (`/videos/`), so the
//! prefix `/videos/` covers the directory listing and everything below it.

use unicode_normalization::UnicodeNormalization;

/// Longest path segment (bytes): the usual file system name limit.
pub const MAX_SEGMENT_BYTES: usize = 255;
/// Longest normalized path (bytes).
pub const MAX_PATH_BYTES: usize = 1024;

/// A normalized storage path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectPath {
    /// `/` (root) or `/a/b` (no trailing slash).
    pub path: String,
    /// The request named a directory (trailing `/`, or the root).
    pub dir: bool,
}

/// Why a path was rejected (400).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("invalid percent-encoding in the path")]
    BadEncoding,
    #[error("the path is not valid UTF-8")]
    NotUtf8,
    #[error("empty path segment (\"//\")")]
    EmptySegment,
    #[error("\".\" and \"..\" are not allowed in paths")]
    DotSegment,
    #[error("control characters are not allowed in paths")]
    ControlChar,
    #[error("\"\\\" and encoded \"/\" are not allowed in names")]
    Separator,
    #[error("names starting with \".rms\" are reserved")]
    Reserved,
    #[error("path segment longer than 255 bytes")]
    SegmentTooLong,
    #[error("path longer than 1024 bytes")]
    TooLong,
    #[error("the path must start with \"/\"")]
    NotAbsolute,
}

impl ObjectPath {
    pub fn root() -> Self {
        Self {
            path: "/".into(),
            dir: true,
        }
    }

    pub fn is_root(&self) -> bool {
        self.path == "/"
    }

    /// The segments (none for the root).
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.path.split('/').filter(|s| !s.is_empty())
    }

    /// Last segment (`""` for the root).
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }

    /// The ancestor directories, outermost first (`/a/b/c` → `/a`, `/a/b`).
    pub fn parents(&self) -> Vec<String> {
        let segs: Vec<&str> = self.segments().collect();
        (1..segs.len())
            .map(|n| format!("/{}", segs[..n].join("/")))
            .collect()
    }

    /// The string matched against permission prefixes: the file path, or the directory
    /// path with a trailing `/` (root: `/`).
    pub fn access_path(&self) -> String {
        if self.dir && !self.is_root() {
            format!("{}/", self.path)
        } else {
            self.path.clone()
        }
    }

    /// The path relative to the bucket directory (no leading `/`).
    pub fn relative(&self) -> &str {
        self.path.trim_start_matches('/')
    }
}

/// Strict percent-decoding of one URL path segment.
fn percent_decode(raw: &str) -> Result<String, PathError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or(PathError::BadEncoding)?;
            let s = std::str::from_utf8(hex).map_err(|_| PathError::BadEncoding)?;
            let b = u8::from_str_radix(s, 16).map_err(|_| PathError::BadEncoding)?;
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PathError::NotUtf8)
}

/// Validate one (decoded) segment; returns it in NFC.
fn check_segment(seg: &str) -> Result<String, PathError> {
    if seg.is_empty() {
        return Err(PathError::EmptySegment);
    }
    let seg: String = seg.nfc().collect();
    if seg == "." || seg == ".." {
        return Err(PathError::DotSegment);
    }
    if seg.chars().any(char::is_control) {
        return Err(PathError::ControlChar);
    }
    if seg.contains(['/', '\\']) {
        return Err(PathError::Separator);
    }
    if seg.get(..4).is_some_and(|p| p.eq_ignore_ascii_case(".rms")) {
        return Err(PathError::Reserved);
    }
    if seg.len() > MAX_SEGMENT_BYTES {
        return Err(PathError::SegmentTooLong);
    }
    Ok(seg)
}

fn build(raw: &str, decode: bool) -> Result<ObjectPath, PathError> {
    let rest = raw.strip_prefix('/').unwrap_or(raw);
    if rest.is_empty() {
        return Ok(ObjectPath::root());
    }
    let (body, dir) = match rest.strip_suffix('/') {
        Some(b) => (b, true),
        None => (rest, false),
    };
    let mut path = String::with_capacity(body.len() + 1);
    for seg in body.split('/') {
        let seg = if decode {
            if seg.is_empty() {
                return Err(PathError::EmptySegment);
            }
            percent_decode(seg)?
        } else {
            seg.to_string()
        };
        path.push('/');
        path.push_str(&check_segment(&seg)?);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    Ok(ObjectPath { path, dir })
}

/// Parse the (still percent-encoded) part of a request path after `/storage/{bucket}`
/// (`""`, `/`, `/videos/`, `/videos/a%20b.mp4`).
pub fn parse_url_path(raw: &str) -> Result<ObjectPath, PathError> {
    build(raw, true)
}

/// Parse a path given in a JSON body (no percent-decoding; the leading `/` is optional).
pub fn parse_plain(raw: &str) -> Result<ObjectPath, PathError> {
    build(raw, false)
}

/// Normalize a permission prefix: must start with `/`; a trailing `/` is kept (`/videos/`
/// covers the directory, `/videos` also `/videos-old.txt`).
pub fn normalize_prefix(raw: &str) -> Result<String, PathError> {
    let raw = raw.trim();
    if !raw.starts_with('/') {
        return Err(PathError::NotAbsolute);
    }
    Ok(parse_plain(raw)?.access_path())
}

/// Percent-encode a normalized path for a URL (segments encoded, `/` kept).
pub fn encode_path(path: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    /// RFC 3986 path segment: everything but unreserved and a few sub-delims.
    const SEGMENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'/')
        .add(b'\\')
        .add(b'^')
        .add(b'|')
        .add(b'[')
        .add(b']')
        .add(b'&')
        .add(b'+')
        .add(b';')
        .add(b'=');
    path.split('/')
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> ObjectPath {
        parse_url_path(raw).unwrap_or_else(|e| panic!("{raw}: {e}"))
    }

    #[test]
    fn normalization() {
        assert_eq!(ok(""), ObjectPath::root());
        assert_eq!(ok("/"), ObjectPath::root());
        assert!(ok("/").is_root());
        let f = ok("/videos/a%20b.mp4");
        assert_eq!((f.path.as_str(), f.dir), ("/videos/a b.mp4", false));
        assert_eq!(f.name(), "a b.mp4");
        assert_eq!(f.parents(), ["/videos"]);
        assert_eq!(f.access_path(), "/videos/a b.mp4");
        assert_eq!(f.relative(), "videos/a b.mp4");
        let d = ok("/videos/2026/");
        assert_eq!((d.path.as_str(), d.dir), ("/videos/2026", true));
        assert_eq!(d.access_path(), "/videos/2026/");
        assert_eq!(d.parents(), ["/videos"]);
        assert_eq!(ok("a/b/c").parents(), ["/a", "/a/b"]);
        assert_eq!(ok("x+y.txt").path, "/x+y.txt", "+ is not a space in paths");
        assert_eq!(ok("/%C3%A9t%C3%A9.mp4").path, "/été.mp4");
        // NFD → NFC (both spellings address the same object).
        assert_eq!(ok("/e%CC%81.mp4").path, "/\u{e9}.mp4");
        assert_eq!(parse_plain("/e\u{301}.mp4").unwrap().path, "/\u{e9}.mp4");
        assert_eq!(ok("/.hidden").path, "/.hidden", "dot files are fine");
        assert_eq!(ok("/a.rms").path, "/a.rms");
    }

    #[test]
    fn rejections() {
        use PathError::*;
        for (raw, err) in [
            ("/a//b", EmptySegment),
            ("//", EmptySegment),
            ("/a/../b", DotSegment),
            ("/%2e%2e/x", DotSegment),
            ("/./x", DotSegment),
            ("/a%2Fb", Separator),
            ("/a%5Cb", Separator),
            ("/a\\b", Separator),
            ("/a%00b", ControlChar),
            ("/a%0Ab", ControlChar),
            ("/a%7F", ControlChar),
            ("/a%C2%85", ControlChar),
            ("/.rms-tmp/x", Reserved),
            ("/x/.RMS", Reserved),
            ("/%ZZ", BadEncoding),
            ("/abc%2", BadEncoding),
            ("/%C3%28", NotUtf8),
        ] {
            assert_eq!(parse_url_path(raw), Err(err), "{raw}");
        }
        assert_eq!(
            parse_url_path(&format!("/{}", "a".repeat(256))),
            Err(SegmentTooLong)
        );
        assert!(parse_url_path(&format!("/{}", "a".repeat(255))).is_ok());
        let long = (0..12)
            .map(|_| "b".repeat(100))
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(parse_url_path(&long), Err(TooLong));
    }

    #[test]
    fn prefixes_and_encoding() {
        assert_eq!(normalize_prefix("/").unwrap(), "/");
        assert_eq!(normalize_prefix("/videos/").unwrap(), "/videos/");
        assert_eq!(normalize_prefix(" /videos ").unwrap(), "/videos");
        assert_eq!(normalize_prefix("videos/"), Err(PathError::NotAbsolute));
        assert_eq!(normalize_prefix("/a/../"), Err(PathError::DotSegment));
        assert_eq!(encode_path("/a b/c#d?.mp4"), "/a%20b/c%23d%3F.mp4");
        assert_eq!(encode_path("/été"), "/%C3%A9t%C3%A9");
        // Round trip.
        let p = "/dir with space/ü+x&y=z.mp4";
        assert_eq!(parse_url_path(&encode_path(p)).unwrap().path, p);
    }
}
