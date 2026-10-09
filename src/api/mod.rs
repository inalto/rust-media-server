//! `/api/v1` router (ARCHITECTURE.md §8): admin API behind the token middleware,
//! auth endpoints and the unauthenticated public player API. Also the JSON view types
//! (`BroadcastView`, `PlaybackUrls`, `EmbedInfo`, ...) shared by the handlers.

pub mod auth;
pub mod broadcasts;
pub mod events;
pub mod metrics;
pub mod public;
pub mod recordings;
pub mod settings;
pub mod system;
pub mod vod;
pub mod waiting;

use axum::Router;
use axum::extract::FromRequest;
use axum::http::{HeaderMap, header};
use axum::middleware;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::db::settings::{UiLanguage, UiTheme};
use crate::db::{self, Broadcast, Recording};
use crate::error::ApiError;
use crate::hub::Hub;
use crate::metrics::ViewerCounts;
use crate::state::AppState;

/// Full `/api/v1` router. Needs the state to build the auth middleware.
pub fn router(state: &AppState) -> Router<AppState> {
    let admin = Router::new()
        .merge(broadcasts::routes())
        .merge(metrics::routes())
        .merge(recordings::routes())
        .merge(waiting::routes(state))
        .merge(settings::routes(state))
        .merge(system::routes())
        .merge(events::routes())
        .merge(vod::routes(state))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));
    let public = public::routes()
        .merge(vod::public_routes())
        .layer(CorsLayer::permissive());
    Router::new().nest("/api/v1", admin.merge(auth::routes()).merge(public))
}

/// JSON body extractor whose rejections are rendered as `{"error": ...}`.
#[derive(Debug, Clone, Copy, Default, FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

// ---------------------------------------------------------------------------
// Base URL resolution
// ---------------------------------------------------------------------------

/// Absolute base URLs for links shown to users.
///
/// From `server.public_url` when set, otherwise `http://<Host header>` with the scheme
/// taken from `X-Forwarded-Proto` (https/http) when present.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseUrl {
    /// e.g. `https://stream.example.com` (no trailing slash)
    pub http: String,
    /// e.g. `wss://stream.example.com`
    pub ws: String,
    /// Host name without port (for the RTMP URL), e.g. `stream.example.com`.
    pub host: String,
    pub https: bool,
}

impl BaseUrl {
    pub fn from_request(config: &Config, headers: &HeaderMap) -> Self {
        if !config.server.public_url.is_empty() {
            let http = config.server.public_url.trim_end_matches('/').to_string();
            let (https, rest) = match http.strip_prefix("https://") {
                Some(rest) => (true, rest.to_string()),
                None => (
                    false,
                    http.strip_prefix("http://").unwrap_or(&http).to_string(),
                ),
            };
            let authority = rest.split('/').next().unwrap_or("localhost");
            return Self {
                ws: format!("{}://{}", if https { "wss" } else { "ws" }, rest),
                host: host_without_port(authority).to_string(),
                http,
                https,
            };
        }
        let authority = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|h| is_valid_authority(h))
            .unwrap_or("localhost");
        let https = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
        let (scheme, ws) = if https {
            ("https", "wss")
        } else {
            ("http", "ws")
        };
        Self {
            http: format!("{scheme}://{authority}"),
            ws: format!("{ws}://{authority}"),
            host: host_without_port(authority).to_string(),
            https,
        }
    }
}

/// Accept only plausible `host[:port]` values (letters, digits, `.-_:[]`).
fn is_valid_authority(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
}

/// `example.com:8080` → `example.com`, `[::1]:8080` → `[::1]`.
fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return match authority.find(']') {
            Some(i) => &authority[..=i],
            None => authority,
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => authority,
    }
}

// ---------------------------------------------------------------------------
// View types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BroadcastStatus {
    Offline,
    Live,
    Paused,
}

impl BroadcastStatus {
    /// §4.2: paused wins over live.
    pub fn of(broadcast: &Broadcast, hub: &Hub) -> Self {
        if broadcast.paused {
            BroadcastStatus::Paused
        } else if hub.is_live(&broadcast.name) {
            BroadcastStatus::Live
        } else {
            BroadcastStatus::Offline
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BroadcastStatus::Offline => "offline",
            BroadcastStatus::Live => "live",
            BroadcastStatus::Paused => "paused",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlaybackUrls {
    pub flv: String,
    pub ws_flv: String,
    pub hls: String,
    pub dash: String,
    /// WHEP endpoint (WebRTC): `POST` an SDP offer here.
    pub whep: String,
    pub embed: String,
    pub player: String,
}

impl PlaybackUrls {
    pub fn new(base: &BaseUrl, name: &str) -> Self {
        Self {
            flv: format!("{}/live/{name}.flv", base.http),
            ws_flv: format!("{}/ws/live/{name}.flv", base.ws),
            hls: format!("{}/hls/{name}/master.m3u8", base.http),
            dash: format!("{}/dash/{name}/manifest.mpd", base.http),
            whep: format!("{}/whep/{name}", base.http),
            embed: format!("{}/embed/{name}", base.http),
            player: format!("{}/player/{name}", base.http),
        }
    }
}

/// What to type into OBS (Settings → Stream → Custom).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ObsSettings {
    /// `rtmp://<host>:<port>/<app>`
    pub server: String,
    pub stream_key: String,
}

impl ObsSettings {
    pub fn new(base: &BaseUrl, config: &Config, stream_key: &str) -> Self {
        Self {
            server: format!(
                "rtmp://{}:{}/{}",
                base.host,
                config.rtmp_port(),
                config.rtmp.app
            ),
            stream_key: stream_key.to_string(),
        }
    }
}

/// Live details of a broadcast (present while a publisher is connected).
#[derive(Debug, Clone, Serialize)]
pub struct LiveInfo {
    pub started_at: DateTime<Utc>,
    pub publisher_ip: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bytes_in: u64,
    pub recording: Option<Recording>,
    /// LL-HLS/LL-DASH packaging status (codec support, readiness, measured GOP).
    pub packager: Option<crate::packager::PackagerStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BroadcastView {
    #[serde(flatten)]
    pub broadcast: Broadcast,
    pub status: BroadcastStatus,
    pub viewers: ViewerCounts,
    pub live: Option<LiveInfo>,
    pub urls: PlaybackUrls,
    pub obs: ObsSettings,
    pub waiting_video_url: Option<String>,
}

/// Per-request context for building views (base URL + global defaults).
pub struct ViewContext {
    pub base: BaseUrl,
    pub default_waiting_video: Option<String>,
    /// Custom global default waiting text (`None` = built-in text of `language`).
    pub default_waiting_text: Option<String>,
    /// UI language setting; selects the built-in waiting text.
    pub language: UiLanguage,
    /// Default colour theme of the dashboard and of the player.
    pub theme: UiTheme,
}

impl ViewContext {
    pub async fn load(state: &AppState, headers: &HeaderMap) -> Result<Self, ApiError> {
        let settings = db::settings::all(&state.db).await?;
        let non_empty = |k: &str| settings.get(k).filter(|v| !v.trim().is_empty()).cloned();
        Ok(Self {
            base: BaseUrl::from_request(&state.config, headers),
            default_waiting_video: non_empty(db::settings::DEFAULT_WAITING_VIDEO),
            // A stored copy of a built-in text is not a customization.
            default_waiting_text: non_empty(db::settings::DEFAULT_WAITING_TEXT)
                .filter(|t| !UiLanguage::is_builtin_waiting_text(t)),
            language: UiLanguage::from_settings(&settings),
            theme: UiTheme::from_settings(&settings),
        })
    }

    /// Global default waiting text in effect: the custom one, else the built-in text
    /// in the UI language.
    pub fn effective_default_waiting_text(&self) -> String {
        self.default_waiting_text
            .clone()
            .unwrap_or_else(|| self.language.builtin_waiting_text().to_string())
    }

    /// Relative waiting video path in effect (broadcast's own, else global default).
    pub fn waiting_video_path(&self, b: &Broadcast) -> Option<String> {
        b.waiting_video
            .clone()
            .filter(|p| !p.trim().is_empty())
            .or_else(|| self.default_waiting_video.clone())
    }

    pub fn waiting_video_url(&self, b: &Broadcast) -> Option<String> {
        self.waiting_video_path(b)
            .map(|p| waiting_url(&self.base, &p))
    }

    /// Waiting text in effect: broadcast's own, else global default, else built-in (in
    /// the UI language). A broadcast text equal to a built-in text (`POST /broadcasts`
    /// stores the Italian one when `waiting_text` is omitted) counts as "no own text",
    /// like `""`; stored texts are never rewritten, only rendered.
    pub fn waiting_text(&self, b: &Broadcast) -> String {
        let own = b.waiting_text.trim();
        if !own.is_empty() && !UiLanguage::is_builtin_waiting_text(own) {
            return b.waiting_text.clone();
        }
        self.effective_default_waiting_text()
    }

    pub fn view(&self, state: &AppState, broadcast: Broadcast) -> BroadcastView {
        let status = BroadcastStatus::of(&broadcast, &state.hub);
        let live = state.hub.get(&broadcast.name).map(|s| {
            let info = s.info();
            LiveInfo {
                started_at: s.started_at,
                publisher_ip: s.publisher_addr.map(|a| a.ip().to_string()),
                video_codec: info.video_codec,
                audio_codec: info.audio_codec,
                width: info.width,
                height: info.height,
                fps: info.fps,
                bytes_in: s.bytes_in(),
                recording: state.recorder.current(&broadcast.name),
                packager: state.packager.status(&broadcast.name),
            }
        });
        BroadcastView {
            status,
            viewers: state.tracker.counts(&broadcast.name),
            live,
            urls: PlaybackUrls::new(&self.base, &broadcast.name),
            obs: ObsSettings::new(&self.base, &state.config, &broadcast.stream_key),
            waiting_video_url: self.waiting_video_url(&broadcast),
            broadcast,
        }
    }
}

/// `/waiting/<relative path>` URL (each path segment percent-encoded).
pub fn waiting_url(base: &BaseUrl, rel: &str) -> String {
    let encoded: Vec<String> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .map(percent_encode_segment)
        .collect();
    format!("{}/waiting/{}", base.http, encoded.join("/"))
}

/// Poster image convention for waiting videos: same relative path with a `.jpg`
/// extension (e.g. `show/waiting.mp4` → `show/waiting.jpg`), next to the video in
/// `data/waiting/`. Returns the relative poster path.
pub fn poster_path_for(video_rel: &str) -> String {
    match video_rel.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() && !stem.ends_with('/') => format!("{stem}.jpg"),
        _ => format!("{video_rel}.jpg"),
    }
}

fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Escape a string for use inside HTML attribute values / single-quoted JS strings.
pub fn escape_html_js(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

/// Pinned CDN player libraries (no frontend build step).
pub mod cdn {
    pub const HLS_JS: &str = "https://cdn.jsdelivr.net/npm/hls.js@1.7.3/dist/hls.min.js";
    pub const MPEGTS_JS: &str = "https://cdn.jsdelivr.net/npm/mpegts.js@1.8.2/dist/mpegts.js";
    pub const DASH_JS: &str =
        "https://cdn.jsdelivr.net/npm/dashjs@5.2.1/dist/modern/umd/dash.all.min.js";
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbedSnippets {
    pub hlsjs: String,
    pub mpegtsjs: String,
    pub dashjs: String,
    /// Vanilla JavaScript WHEP (WebRTC) client, no library.
    pub whep: String,
}

/// `GET /broadcasts/{name}/embed`: ready-to-paste embed code.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedInfo {
    pub iframe: String,
    pub urls: PlaybackUrls,
    pub obs: ObsSettings,
    pub snippets: EmbedSnippets,
}

impl EmbedInfo {
    pub fn new(urls: PlaybackUrls, obs: ObsSettings) -> Self {
        let e = |s: &str| escape_html_js(s);
        let video = r#"<video id="rms-player" controls autoplay muted playsinline style="width:100%;max-width:960px;background:#000"></video>"#;
        let iframe = format!(
            r#"<iframe src="{}" width="640" height="360" style="border:0;max-width:100%;aspect-ratio:16/9;height:auto" allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>"#,
            e(&urls.embed)
        );
        let hlsjs = format!(
            r#"{video}
<script src="{lib}"></script>
<script>
  (function () {{
    var video = document.getElementById('rms-player');
    var src = '{src}';
    if (window.Hls && Hls.isSupported()) {{
      var hls = new Hls({{ lowLatencyMode: true }});
      hls.loadSource(src);
      hls.attachMedia(video);
    }} else if (video.canPlayType('application/vnd.apple.mpegurl')) {{
      video.src = src; // Safari / iOS: native LL-HLS
    }}
  }})();
</script>"#,
            lib = cdn::HLS_JS,
            src = e(&urls.hls)
        );
        let mpegtsjs = format!(
            r#"{video}
<script src="{lib}"></script>
<script>
  (function () {{
    if (!window.mpegts || !mpegts.getFeatureList().mseLivePlayback) return;
    var player = mpegts.createPlayer(
      {{ type: 'flv', isLive: true, url: '{src}' }},
      {{ enableStashBuffer: false, liveBufferLatencyChasing: true }}
    );
    player.attachMediaElement(document.getElementById('rms-player'));
    player.load();
    player.play();
  }})();
</script>"#,
            lib = cdn::MPEGTS_JS,
            src = e(&urls.flv)
        );
        let dashjs = format!(
            r#"{video}
<script src="{lib}"></script>
<script>
  (function () {{
    var player = dashjs.MediaPlayer().create();
    player.updateSettings({{ streaming: {{ delay: {{ liveDelay: 2 }}, liveCatchup: {{ enabled: true }} }} }});
    player.initialize(document.getElementById('rms-player'), '{src}', true);
  }})();
</script>"#,
            lib = cdn::DASH_JS,
            src = e(&urls.dash)
        );
        let whep = format!(
            r#"{video}
<script>
  (async function () {{
    var video = document.getElementById('rms-player');
    var endpoint = '{src}';
    var pc = new RTCPeerConnection();
    var stream = new MediaStream();
    pc.addTransceiver('video', {{ direction: 'recvonly' }});
    pc.addTransceiver('audio', {{ direction: 'recvonly' }});
    pc.ontrack = function (e) {{ stream.addTrack(e.track); video.srcObject = stream; }};
    var offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    var res = await fetch(endpoint, {{
      method: 'POST', headers: {{ 'Content-Type': 'application/sdp' }}, body: pc.localDescription.sdp
    }});
    if (res.status !== 201) {{ console.error('WHEP error', res.status, await res.text()); pc.close(); return; }}
    var session = new URL(res.headers.get('Location'), endpoint).href;
    await pc.setRemoteDescription({{ type: 'answer', sdp: await res.text() }});
    window.addEventListener('pagehide', function () {{
      fetch(session, {{ method: 'DELETE', keepalive: true }}); pc.close();
    }});
  }})();
</script>"#,
            src = e(&urls.whep)
        );
        Self {
            iframe,
            urls,
            obs,
            snippets: EmbedSnippets {
                hlsjs,
                mpegtsjs,
                dashjs,
                whep,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn base_url_from_host_and_proto() {
        let cfg = Config::default();
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("example.com:8080"));
        let b = BaseUrl::from_request(&cfg, &h);
        assert_eq!(b.http, "http://example.com:8080");
        assert_eq!(b.ws, "ws://example.com:8080");
        assert_eq!(b.host, "example.com");
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let b = BaseUrl::from_request(&cfg, &h);
        assert_eq!(b.http, "https://example.com:8080");
        assert_eq!(b.ws, "wss://example.com:8080");
        h.insert(header::HOST, HeaderValue::from_static("evil\"<host>"));
        assert_eq!(BaseUrl::from_request(&cfg, &h).host, "localhost");
        h.insert(header::HOST, HeaderValue::from_static("[::1]:8080"));
        assert_eq!(BaseUrl::from_request(&cfg, &h).host, "[::1]");
    }

    #[test]
    fn base_url_from_public_url() {
        let mut cfg = Config::default();
        cfg.server.public_url = "https://stream.example.com".into();
        let b = BaseUrl::from_request(&cfg, &HeaderMap::new());
        assert_eq!(b.http, "https://stream.example.com");
        assert_eq!(b.ws, "wss://stream.example.com");
        assert_eq!(b.host, "stream.example.com");
        let urls = PlaybackUrls::new(&b, "show");
        assert_eq!(urls.flv, "https://stream.example.com/live/show.flv");
        assert_eq!(urls.ws_flv, "wss://stream.example.com/ws/live/show.flv");
        assert_eq!(urls.hls, "https://stream.example.com/hls/show/master.m3u8");
        assert_eq!(
            urls.dash,
            "https://stream.example.com/dash/show/manifest.mpd"
        );
        assert_eq!(urls.whep, "https://stream.example.com/whep/show");
        assert_eq!(urls.embed, "https://stream.example.com/embed/show");
        assert_eq!(urls.player, "https://stream.example.com/player/show");
        let obs = ObsSettings::new(&b, &cfg, "k");
        assert_eq!(obs.server, "rtmp://stream.example.com:1935/live");
    }

    #[test]
    fn waiting_paths() {
        let b = BaseUrl {
            http: "http://h".into(),
            ws: "ws://h".into(),
            host: "h".into(),
            https: false,
        };
        assert_eq!(
            waiting_url(&b, "show/a b.mp4"),
            "http://h/waiting/show/a%20b.mp4"
        );
        assert_eq!(waiting_url(&b, "../x.mp4"), "http://h/waiting/x.mp4");
        assert_eq!(poster_path_for("show/waiting.mp4"), "show/waiting.jpg");
        assert_eq!(poster_path_for("noext"), "noext.jpg");
    }

    #[test]
    fn embed_snippets_reference_urls() {
        let b = BaseUrl::from_request(&Config::default(), &HeaderMap::new());
        let urls = PlaybackUrls::new(&b, "show");
        let e = EmbedInfo::new(urls.clone(), ObsSettings::new(&b, &Config::default(), "k"));
        assert!(e.iframe.contains(&urls.embed));
        assert!(e.iframe.contains("allow=\"autoplay; fullscreen"));
        assert!(e.snippets.hlsjs.contains(&urls.hls) && e.snippets.hlsjs.contains("hls.js@1.7.3"));
        assert!(e.snippets.mpegtsjs.contains(&urls.flv));
        assert!(e.snippets.dashjs.contains(&urls.dash));
        assert!(e.snippets.whep.contains(&urls.whep));
        assert!(e.snippets.whep.contains("application/sdp"));
        assert!(e.snippets.whep.contains("recvonly"));
    }
}
