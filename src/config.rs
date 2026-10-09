//! Configuration: defaults < `config.toml` < `RMS_<SECTION>_<KEY>` env vars < CLI flags.
//!
//! Defaults are exactly those of ARCHITECTURE.md §10. Every section is optional in
//! the TOML file and every key inside a section is optional too.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Root configuration object.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub rtmp: RtmpConfig,
    pub hls: HlsConfig,
    pub dash: DashConfig,
    pub recording: RecordingConfig,
    pub metrics: MetricsConfig,
    pub ffmpeg: FfmpegConfig,
    pub log: LogConfig,
    pub webrtc: WebRtcConfig,
    pub vod: VodConfig,
    pub cluster: ClusterConfig,
    pub storage: StorageConfig,
}

/// Bucket storage of the users (`PUT /storage/{bucket}/...`, docs/ACCOUNTS.md §3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct StorageConfig {
    /// Largest file accepted by the storage API, MiB (413 above it).
    pub max_object_mb: u64,
    /// Directory of the bucket files (`<root>/<bucket id>/<path>`); empty =
    /// `<data_dir>/buckets`. Shared by every node in cluster mode.
    pub root: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_object_mb: 20480,
            root: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    pub http_bind: String,
    /// e.g. "https://stream.example.com"; empty = derive from the request `Host` header.
    pub public_url: String,
    /// Empty = generate at first start (stored in `<data_dir>/admin_token`).
    pub admin_token: String,
    pub data_dir: PathBuf,
    /// Honour `X-Forwarded-For` / `X-Real-IP` for viewer IPs.
    pub trust_proxy: bool,
    pub max_upload_mb: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_bind: "0.0.0.0:8080".into(),
            public_url: String::new(),
            admin_token: String::new(),
            data_dir: PathBuf::from("./data"),
            trust_proxy: false,
            max_upload_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RtmpConfig {
    pub bind: String,
    /// Advertised application name (OBS "Server" URL suffix). Any app is accepted on ingest.
    pub app: String,
    /// Maximum simultaneous RTMP connections (handshaking + publishing). Extra connections
    /// are closed immediately; protects against unauthenticated memory exhaustion.
    pub max_connections: usize,
}

impl Default for RtmpConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:1935".into(),
            app: "live".into(),
            max_connections: 256,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HlsConfig {
    pub enabled: bool,
    pub part_duration: f64,
    pub max_segment_duration: f64,
    pub window_segments: u32,
    pub linger_secs: u64,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            part_duration: 0.5,
            max_segment_duration: 6.0,
            window_segments: 6,
            linger_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DashConfig {
    pub enabled: bool,
    pub segment_duration: f64,
}

impl Default for DashConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_duration: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecordingConfig {
    /// Needs ffmpeg.
    pub remux_to_mp4: bool,
    pub keep_flv: bool,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            remux_to_mp4: true,
            keep_flv: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MetricsConfig {
    pub sample_interval_secs: u64,
    pub session_timeout_secs: u64,
    pub retention_days: u64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            sample_interval_secs: 5,
            session_timeout_secs: 30,
            retention_days: 90,
        }
    }
}

/// WebRTC egress via WHEP (`/whep/{name}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebRtcConfig {
    pub enabled: bool,
    /// Single UDP port shared by every WebRTC session (ICE, DTLS, SRTP).
    pub udp_port: u16,
    /// Port advertised in the ICE candidates when it differs from `udp_port` (NAT port
    /// mapping, e.g. several nodes behind one public IP: public 8444 → node port 8443).
    /// `0` = same as `udp_port`.
    pub public_port: u16,
    /// Public IPv4 addresses advertised as ICE host candidates (1:1 NAT, e.g. a cloud
    /// VM's elastic IP). Empty = the addresses of the local network interfaces.
    pub public_ip: Vec<String>,
    /// ICE-lite: the server only answers connectivity checks (it has a reachable address).
    pub ice_lite: bool,
    /// Maximum concurrent WebRTC sessions (all broadcasts); further offers get 503.
    pub max_sessions: usize,
    /// Opus bitrate of the AAC → Opus transcode (ffmpeg).
    pub audio_bitrate_kbps: u32,
    /// Stop the per-broadcast WebRTC publisher after this many seconds without sessions.
    pub idle_stop_secs: u64,
    /// Optional `stun:host:port` advertised to WHEP clients in a `Link` header.
    pub stun_server: String,
}

impl WebRtcConfig {
    /// Port advertised to viewers (ICE host candidates): `public_port`, or `udp_port`
    /// when it is 0.
    pub fn advertised_port(&self) -> u16 {
        if self.public_port != 0 {
            self.public_port
        } else {
            self.udp_port
        }
    }
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            udp_port: 8443,
            public_port: 0,
            public_ip: Vec::new(),
            ice_lite: true,
            max_sessions: 100,
            audio_bitrate_kbps: 96,
            idle_stop_secs: 30,
            stun_server: String::new(),
        }
    }
}

/// Video on demand (`src/vod/`): uploaded or inbox files transcoded in one ffmpeg pass to a
/// multi-bitrate CMAF ladder served as HLS and DASH.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VodConfig {
    /// `false` stops uploads, the inbox scanner and the transcoding jobs (existing videos
    /// stay playable).
    pub enabled: bool,
    /// Watched folder (files dropped there are imported). Empty = `<data_dir>/vod/inbox`.
    /// Sub-folders named after a ladder (`inbox/mobile/`) select that ladder.
    pub inbox_dir: String,
    pub scan_interval_secs: u64,
    /// Concurrent transcoding jobs (each is one ffmpeg process encoding every rendition).
    pub max_parallel_jobs: usize,
    /// Segment duration and keyframe interval (s).
    pub segment_duration: f64,
    /// ffmpeg video encoder: `libx264` (default), `h264_nvenc`, `h264_qsv`, `h264_vaapi`...
    pub encoder: String,
    /// Extra output-side ffmpeg arguments for the encoder (split on whitespace).
    pub encoder_args: String,
    /// Extra input-side ffmpeg arguments, before `-i` (e.g. `-hwaccel cuda`, `-vaapi_device /dev/dri/renderD128`).
    pub hwaccel_args: String,
    /// Encoder preset (`veryfast` for libx264/qsv, `p4` for nvenc); empty = no `-preset`.
    pub preset: String,
    /// Ladder used when none is chosen (upload without `ladder`, files in the inbox root).
    pub default_ladder: String,
    /// Keep the original file after a successful transcode (needed to re-transcode and to
    /// download it).
    pub keep_source: bool,
    pub ladders: Vec<VodLadder>,
}

/// A named set of renditions (multi-bitrate ladder).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VodLadder {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub renditions: Vec<VodRendition>,
}

/// One rendition of a ladder. `height` is the size of the short side (the height of a
/// landscape video, the width of a portrait one); `0` = the source size. `video_kbps` `0`
/// = constant quality (CRF 23) instead of a target bitrate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VodRendition {
    pub name: String,
    pub height: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

impl VodRendition {
    fn new(name: &str, height: u32, video_kbps: u32, audio_kbps: u32) -> Self {
        Self {
            name: name.into(),
            height,
            video_kbps,
            audio_kbps,
        }
    }
}

impl VodConfig {
    /// The built-in ladders (also in `config.example.toml`).
    pub fn default_ladders() -> Vec<VodLadder> {
        vec![
            VodLadder {
                name: "standard".into(),
                description: "1080p → 360p, per desktop e mobile".into(),
                renditions: vec![
                    VodRendition::new("1080p", 1080, 5000, 160),
                    VodRendition::new("720p", 720, 2800, 128),
                    VodRendition::new("480p", 480, 1400, 96),
                    VodRendition::new("360p", 360, 800, 96),
                ],
            },
            VodLadder {
                name: "mobile".into(),
                description: "720p e 360p, leggero per le reti mobili".into(),
                renditions: vec![
                    VodRendition::new("720p", 720, 2200, 128),
                    VodRendition::new("360p", 360, 700, 64),
                ],
            },
            VodLadder {
                name: "single".into(),
                description: "Una sola qualità, alla risoluzione originale (CRF 23)".into(),
                renditions: vec![VodRendition::new("source", 0, 0, 128)],
            },
        ]
    }

    /// Ladder by name.
    pub fn ladder(&self, name: &str) -> Option<&VodLadder> {
        self.ladders.iter().find(|l| l.name == name)
    }

    /// Inbox folder: `inbox_dir`, or `<data_dir>/vod/inbox` when empty.
    pub fn inbox_path(&self, data_dir: &Path) -> PathBuf {
        if self.inbox_dir.trim().is_empty() {
            data_dir.join("vod").join("inbox")
        } else {
            PathBuf::from(self.inbox_dir.trim())
        }
    }

    /// Startup validation: clear messages naming the offending key.
    pub fn validate(&self) -> anyhow::Result<()> {
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 32
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                && !s.starts_with('.')
        };
        if self.scan_interval_secs == 0 {
            bail!("vod.scan_interval_secs must be at least 1");
        }
        if !(1..=16).contains(&self.max_parallel_jobs) {
            bail!("vod.max_parallel_jobs must be between 1 and 16");
        }
        if !(self.segment_duration.is_finite() && (1.0..=20.0).contains(&self.segment_duration)) {
            bail!("vod.segment_duration must be between 1 and 20 seconds");
        }
        if self.encoder.trim().is_empty() {
            bail!("vod.encoder must not be empty (e.g. \"libx264\")");
        }
        if self.ladders.is_empty() {
            bail!("vod.ladders: at least one ladder is required");
        }
        let mut names = std::collections::HashSet::new();
        for l in &self.ladders {
            if !safe(&l.name) || l.name.starts_with('_') {
                bail!(
                    "vod.ladders: invalid ladder name {:?} (letters, digits, '-', '_', '.', max 32, not starting with '_' or '.'; it is also the inbox sub-folder name)",
                    l.name
                );
            }
            if !names.insert(l.name.as_str()) {
                bail!("vod.ladders: duplicate ladder name {:?}", l.name);
            }
            if l.renditions.is_empty() {
                bail!("vod.ladders.{}: at least one rendition is required", l.name);
            }
            let mut rnames = std::collections::HashSet::new();
            let mut prev: Option<u32> = None;
            for r in &l.renditions {
                if !safe(&r.name) {
                    bail!(
                        "vod.ladders.{}: invalid rendition name {:?}",
                        l.name,
                        r.name
                    );
                }
                if !rnames.insert(r.name.as_str()) {
                    bail!(
                        "vod.ladders.{}: duplicate rendition name {:?}",
                        l.name,
                        r.name
                    );
                }
                if r.height != 0 && !(16..=8640).contains(&r.height) {
                    bail!(
                        "vod.ladders.{}.{}: height must be 0 (source size) or between 16 and 8640",
                        l.name,
                        r.name
                    );
                }
                // Sorted from the largest to the smallest; 0 (source size) can only be first.
                let size = if r.height == 0 { u32::MAX } else { r.height };
                if let Some(p) = prev
                    && size >= p
                {
                    bail!(
                        "vod.ladders.{}: renditions must be sorted by height, largest first (height 0 = source size, only as the first one)",
                        l.name
                    );
                }
                prev = Some(size);
                if r.video_kbps != 0 && !(50..=200_000).contains(&r.video_kbps) {
                    bail!(
                        "vod.ladders.{}.{}: video_kbps must be 0 (constant quality) or between 50 and 200000",
                        l.name,
                        r.name
                    );
                }
                if !(16..=512).contains(&r.audio_kbps) {
                    bail!(
                        "vod.ladders.{}.{}: audio_kbps must be between 16 and 512",
                        l.name,
                        r.name
                    );
                }
            }
        }
        if self.ladder(&self.default_ladder).is_none() {
            bail!(
                "vod.default_ladder {:?} is not one of the configured ladders",
                self.default_ladder
            );
        }
        Ok(())
    }
}

impl Default for VodConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            inbox_dir: String::new(),
            scan_interval_secs: 10,
            max_parallel_jobs: 1,
            segment_duration: 4.0,
            encoder: "libx264".into(),
            encoder_args: String::new(),
            hwaccel_args: String::new(),
            preset: "veryfast".into(),
            default_ladder: "standard".into(),
            keep_source: true,
            ladders: Self::default_ladders(),
        }
    }
}

/// Multi-instance mode (docs/CLUSTER.md §3.1). `enabled = false` (default) = single node:
/// SQLite, no registry, no internal listener.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClusterConfig {
    pub enabled: bool,
    /// Unique, stable node name (`[A-Za-z0-9._-]{1,64}`); empty = the host name.
    pub node_id: String,
    /// `postgres://user:password@host:5432/db` (required in cluster mode).
    pub database_url: String,
    /// Internal URL of this node reachable by the other nodes, e.g. `http://rms1:8081`.
    pub advertise_url: String,
    /// Listener of the internal routes (`/internal/*`), separate from the public one.
    pub internal_bind: String,
    /// Shared secret of the internal routes (≥ 32 characters, identical on every node).
    pub internal_secret: String,
    pub heartbeat_secs: u64,
    /// A node (and its streams) is considered dead after this many seconds without
    /// heartbeat.
    pub node_ttl_secs: u64,
    /// Stop a relay without local viewers after this many seconds (phase 7C-2).
    pub relay_idle_secs: u64,
    /// Public WHEP base of this node announced in `nodes.whep_url` (e.g.
    /// `https://rms2.example.com/whep`); empty = `<server.public_url>/whep`.
    pub whep_url: String,
    /// A node without usable WebRTC answers `POST /whep/{name}` with `307` to another
    /// node's `whep_url` (only to a base different from its own).
    pub whep_redirect: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: String::new(),
            database_url: String::new(),
            advertise_url: String::new(),
            internal_bind: "0.0.0.0:8081".into(),
            internal_secret: String::new(),
            heartbeat_secs: 5,
            node_ttl_secs: 15,
            relay_idle_secs: 30,
            whep_url: String::new(),
            whep_redirect: true,
        }
    }
}

/// Secrets are redacted (the configuration may end up in logs).
impl std::fmt::Debug for ClusterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterConfig")
            .field("enabled", &self.enabled)
            .field("node_id", &self.node_id)
            .field("database_url", &redact_url_password(&self.database_url))
            .field("advertise_url", &self.advertise_url)
            .field("internal_bind", &self.internal_bind)
            .field(
                "internal_secret",
                &if self.internal_secret.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("heartbeat_secs", &self.heartbeat_secs)
            .field("node_ttl_secs", &self.node_ttl_secs)
            .field("relay_idle_secs", &self.relay_idle_secs)
            .field("whep_url", &self.whep_url)
            .field("whep_redirect", &self.whep_redirect)
            .finish()
    }
}

/// `postgres://user:secret@host/db` → `postgres://user:***@host/db` (for logs).
pub fn redact_url_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let user = userinfo.split(':').next().unwrap_or("");
            if userinfo.contains(':') {
                format!("{scheme}://{user}:***@{host}{path}")
            } else {
                format!("{scheme}://{user}@{host}{path}")
            }
        }
        None => url.to_string(),
    }
}

/// Host name of the machine (`/proc/sys/kernel/hostname`, then `$HOSTNAME`), `rms` when
/// unknown. Default of `cluster.node_id`.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "rms".to_string())
}

/// `[A-Za-z0-9._-]{1,64}`: node ids end up in URLs, session ids and log lines.
pub fn is_valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl ClusterConfig {
    /// Startup validation of cluster mode (no-op when disabled); clear messages naming
    /// the offending key.
    pub fn validate(&self, server: &ServerConfig) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !is_valid_node_id(&self.node_id) {
            bail!(
                "cluster.node_id {:?} is invalid: 1-64 characters among letters, digits, '.', '_', '-' (RMS_CLUSTER_NODE_ID)",
                self.node_id
            );
        }
        if !(self.database_url.starts_with("postgres://")
            || self.database_url.starts_with("postgresql://"))
        {
            bail!(
                "cluster.enabled requires cluster.database_url = postgres://user:password@host:5432/db (RMS_CLUSTER_DATABASE_URL)"
            );
        }
        if !(self.advertise_url.starts_with("http://")
            || self.advertise_url.starts_with("https://"))
        {
            bail!(
                "cluster.enabled requires cluster.advertise_url, the internal URL of this node reachable by the others, e.g. http://rms1:8081 (RMS_CLUSTER_ADVERTISE_URL)"
            );
        }
        self.internal_bind
            .parse::<SocketAddr>()
            .with_context(|| format!("invalid cluster.internal_bind {:?}", self.internal_bind))?;
        if self.internal_secret.chars().count() < 32 {
            bail!(
                "cluster.internal_secret must be at least 32 characters, identical on every node (RMS_CLUSTER_INTERNAL_SECRET); e.g. `openssl rand -hex 32`"
            );
        }
        if server.admin_token.is_empty() {
            bail!(
                "cluster.enabled requires server.admin_token (RMS_ADMIN_TOKEN), identical on every node: data/admin_token is per node and is not generated in cluster mode"
            );
        }
        if self.heartbeat_secs == 0 {
            bail!("cluster.heartbeat_secs must be at least 1");
        }
        if self.node_ttl_secs < 2 * self.heartbeat_secs {
            bail!("cluster.node_ttl_secs must be at least twice cluster.heartbeat_secs");
        }
        if self.relay_idle_secs == 0 {
            bail!("cluster.relay_idle_secs must be at least 1");
        }
        if !self.whep_url.is_empty()
            && !(self.whep_url.starts_with("http://") || self.whep_url.starts_with("https://"))
        {
            bail!("cluster.whep_url must start with http:// or https:// (RMS_CLUSTER_WHEP_URL)");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FfmpegConfig {
    /// Empty = search `PATH`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LogConfig {
    /// `RUST_LOG` overrides this.
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

/// Values given on the command line; they win over everything else.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub data_dir: Option<PathBuf>,
    pub http_bind: Option<String>,
    pub rtmp_bind: Option<String>,
}

impl Config {
    /// Load the configuration.
    ///
    /// `path`: explicit `--config` file (must exist). When `None`, `./config.toml` is used
    /// if present, otherwise built-in defaults. Returns the config plus a list of
    /// warnings to log once tracing is initialised.
    pub fn load(path: Option<&Path>, cli: &CliOverrides) -> anyhow::Result<(Config, Vec<String>)> {
        let mut warnings = Vec::new();
        let mut config = match path {
            Some(p) => Self::from_file(p)?,
            None => {
                let default_path = Path::new("config.toml");
                if default_path.exists() {
                    Self::from_file(default_path)?
                } else {
                    Config::default()
                }
            }
        };
        config = apply_env_overrides(config, std::env::vars(), &mut warnings)?;
        config.apply_cli(cli);
        config.normalize();
        config.validate()?;
        Ok((config, warnings))
    }

    pub fn from_file(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::from_toml_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))
    }

    pub fn from_toml_str(text: &str) -> anyhow::Result<Config> {
        Ok(toml::from_str(text)?)
    }

    pub fn apply_cli(&mut self, cli: &CliOverrides) {
        if let Some(d) = &cli.data_dir {
            self.server.data_dir = d.clone();
        }
        if let Some(b) = &cli.http_bind {
            self.server.http_bind = b.clone();
        }
        if let Some(b) = &cli.rtmp_bind {
            self.rtmp.bind = b.clone();
        }
    }

    fn normalize(&mut self) {
        self.server.public_url = self
            .server
            .public_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        self.server.admin_token = self.server.admin_token.trim().to_string();
        self.rtmp.app = self.rtmp.app.trim().trim_matches('/').to_string();
        if self.rtmp.app.is_empty() {
            self.rtmp.app = "live".into();
        }
        self.webrtc.public_ip = self
            .webrtc
            .public_ip
            .iter()
            .map(|ip| ip.trim().to_string())
            .filter(|ip| !ip.is_empty())
            .collect();
        self.webrtc.stun_server = self.webrtc.stun_server.trim().to_string();
        self.vod.encoder = self.vod.encoder.trim().to_string();
        self.vod.preset = self.vod.preset.trim().to_string();
        self.vod.default_ladder = self.vod.default_ladder.trim().to_string();
        let c = &mut self.cluster;
        c.node_id = c.node_id.trim().to_string();
        if c.node_id.is_empty() {
            c.node_id = hostname();
        }
        c.database_url = c.database_url.trim().to_string();
        c.advertise_url = c.advertise_url.trim().trim_end_matches('/').to_string();
        c.internal_bind = c.internal_bind.trim().to_string();
        c.internal_secret = c.internal_secret.trim().to_string();
        c.whep_url = c.whep_url.trim().trim_end_matches('/').to_string();
        self.storage.root = self.storage.root.trim().to_string();
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.http_bind_addr()?;
        self.rtmp_bind_addr()?;
        if !self.server.public_url.is_empty()
            && !(self.server.public_url.starts_with("http://")
                || self.server.public_url.starts_with("https://"))
        {
            bail!("server.public_url must start with http:// or https://");
        }
        if self.hls.part_duration <= 0.0 || self.hls.max_segment_duration <= 0.0 {
            bail!("hls durations must be positive");
        }
        if self.dash.segment_duration <= 0.0 {
            bail!("dash.segment_duration must be positive");
        }
        if self.metrics.sample_interval_secs == 0 || self.metrics.session_timeout_secs == 0 {
            bail!("metrics intervals must be positive");
        }
        if self.webrtc.enabled && self.webrtc.udp_port == 0 {
            bail!("webrtc.udp_port must be a fixed port (not 0)");
        }
        for ip in &self.webrtc.public_ip {
            if ip.parse::<std::net::Ipv4Addr>().is_err() {
                bail!("webrtc.public_ip: {ip:?} is not an IPv4 address");
            }
        }
        if !(6..=510).contains(&self.webrtc.audio_bitrate_kbps) {
            bail!("webrtc.audio_bitrate_kbps must be between 6 and 510");
        }
        self.vod.validate()?;
        self.cluster.validate(&self.server)?;
        if self.storage.max_object_mb == 0 {
            bail!("storage.max_object_mb must be positive");
        }
        Ok(())
    }

    pub fn http_bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.server
            .http_bind
            .parse()
            .with_context(|| format!("invalid server.http_bind {:?}", self.server.http_bind))
    }

    pub fn rtmp_bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.rtmp
            .bind
            .parse()
            .with_context(|| format!("invalid rtmp.bind {:?}", self.rtmp.bind))
    }

    /// RTMP port advertised to encoders (from `rtmp.bind`, default 1935).
    pub fn rtmp_port(&self) -> u16 {
        self.rtmp_bind_addr().map(|a| a.port()).unwrap_or(1935)
    }

    pub fn data_dir(&self) -> &Path {
        &self.server.data_dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.server.data_dir.join("rms.sqlite")
    }

    pub fn waiting_dir(&self) -> PathBuf {
        self.server.data_dir.join("waiting")
    }

    pub fn recordings_dir(&self) -> PathBuf {
        self.server.data_dir.join("recordings")
    }

    /// `<data_dir>/vod`: one sub-directory per video on demand.
    pub fn vod_dir(&self) -> PathBuf {
        self.server.data_dir.join("vod")
    }

    /// The VOD inbox folder (see [`VodConfig::inbox_path`]).
    pub fn vod_inbox_dir(&self) -> PathBuf {
        self.vod.inbox_path(&self.server.data_dir)
    }

    /// Root of the bucket files: `storage.root`, else `<data_dir>/buckets`.
    pub fn storage_root(&self) -> PathBuf {
        if self.storage.root.is_empty() {
            self.server.data_dir.join("buckets")
        } else {
            PathBuf::from(&self.storage.root)
        }
    }

    pub fn admin_token_path(&self) -> PathBuf {
        self.server.data_dir.join("admin_token")
    }

    /// Resolve the ffmpeg executable: `ffmpeg.path` when set (and existing), otherwise
    /// a search of `PATH`. `None` when ffmpeg is unavailable (features must degrade).
    pub fn ffmpeg_path(&self) -> Option<PathBuf> {
        let configured = self.ffmpeg.path.trim();
        if !configured.is_empty() {
            let p = PathBuf::from(configured);
            return p.is_file().then_some(p);
        }
        find_in_path("ffmpeg")
    }
}

/// Search `PATH` for an executable file named `name`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Apply `RMS_<SECTION>_<KEY>` overrides (e.g. `RMS_SERVER_HTTP_BIND`, `RMS_HLS_PART_DURATION`).
/// `RMS_ADMIN_TOKEN` is accepted as an alias of `RMS_SERVER_ADMIN_TOKEN` (§8).
///
/// Values are parsed according to the type of the existing key (string, integer,
/// float, bool), so `RMS_SERVER_ADMIN_TOKEN=1234` stays a string. Unknown keys
/// produce a warning and are ignored.
pub fn apply_env_overrides(
    config: Config,
    vars: impl IntoIterator<Item = (String, String)>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Config> {
    let mut table = toml::Table::try_from(&config).context("serializing config")?;
    let mut changed = false;
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix("RMS_") else {
            continue;
        };
        let rest = rest.to_ascii_lowercase();
        let (section, key) = if rest == "admin_token" {
            ("server".to_string(), "admin_token".to_string())
        } else {
            match rest.split_once('_') {
                Some((s, k)) if !k.is_empty() => (s.to_string(), k.to_string()),
                _ => {
                    warnings.push(format!("ignoring unknown config env var {name}"));
                    continue;
                }
            }
        };
        let Some(toml::Value::Table(sec)) = table.get_mut(&section) else {
            warnings.push(format!(
                "ignoring unknown config env var {name} (no section [{section}])"
            ));
            continue;
        };
        let Some(current) = sec.get(&key) else {
            warnings.push(format!(
                "ignoring unknown config env var {name} (no key {section}.{key})"
            ));
            continue;
        };
        let new_value = match current {
            toml::Value::String(_) => toml::Value::String(value.clone()),
            toml::Value::Integer(_) => toml::Value::Integer(
                value
                    .trim()
                    .parse::<i64>()
                    .with_context(|| format!("{name}: expected an integer, got {value:?}"))?,
            ),
            toml::Value::Float(_) => toml::Value::Float(
                value
                    .trim()
                    .parse::<f64>()
                    .with_context(|| format!("{name}: expected a number, got {value:?}"))?,
            ),
            toml::Value::Boolean(_) => toml::Value::Boolean(
                parse_bool(&value)
                    .with_context(|| format!("{name}: expected a boolean, got {value:?}"))?,
            ),
            toml::Value::Array(items) if items.iter().any(|v| v.is_table()) => bail!(
                "{name}: lists of tables (e.g. vod.ladders) can only be set in the config file"
            ),
            // Lists of strings (e.g. `RMS_WEBRTC_PUBLIC_IP=203.0.113.7,198.51.100.2`).
            toml::Value::Array(_) => toml::Value::Array(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| toml::Value::String(v.to_string()))
                    .collect(),
            ),
            other => bail!(
                "{name}: cannot override non-scalar key of type {}",
                other.type_str()
            ),
        };
        sec.insert(key, new_value);
        changed = true;
    }
    if !changed {
        return Ok(config);
    }
    toml::Value::Table(table)
        .try_into()
        .context("applying env overrides")
}

fn parse_bool(v: &str) -> anyhow::Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        _ => bail!("invalid boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_match_contract() {
        let c = Config::default();
        assert_eq!(c.storage.max_object_mb, 20480);
        assert_eq!(c.storage.root, "");
        assert_eq!(c.storage_root(), PathBuf::from("./data/buckets"));
        assert_eq!(c.server.http_bind, "0.0.0.0:8080");
        assert_eq!(c.server.public_url, "");
        assert_eq!(c.server.data_dir, PathBuf::from("./data"));
        assert!(!c.server.trust_proxy);
        assert_eq!(c.server.max_upload_mb, 1024);
        assert_eq!(c.rtmp.bind, "0.0.0.0:1935");
        assert_eq!(c.rtmp.app, "live");
        assert!(c.hls.enabled);
        assert_eq!(c.hls.part_duration, 0.5);
        assert_eq!(c.hls.max_segment_duration, 6.0);
        assert_eq!(c.hls.window_segments, 6);
        assert_eq!(c.hls.linger_secs, 30);
        assert_eq!(c.dash.segment_duration, 2.0);
        assert!(c.recording.remux_to_mp4);
        assert!(!c.recording.keep_flv);
        assert_eq!(c.metrics.sample_interval_secs, 5);
        assert_eq!(c.metrics.session_timeout_secs, 30);
        assert_eq!(c.metrics.retention_days, 90);
        assert_eq!(c.ffmpeg.path, "");
        assert_eq!(c.log.level, "info");
        assert!(c.webrtc.enabled);
        assert_eq!(c.webrtc.udp_port, 8443);
        assert!(c.webrtc.public_ip.is_empty());
        assert!(c.webrtc.ice_lite);
        assert_eq!(c.webrtc.max_sessions, 100);
        assert_eq!(c.webrtc.audio_bitrate_kbps, 96);
        assert_eq!(c.webrtc.idle_stop_secs, 30);
        assert_eq!(c.webrtc.stun_server, "");
    }

    #[test]
    fn webrtc_env_overrides_and_validation() {
        let mut warnings = Vec::new();
        let mut c = apply_env_overrides(
            Config::default(),
            vars(&[
                ("RMS_WEBRTC_UDP_PORT", "18443"),
                ("RMS_WEBRTC_PUBLIC_IP", " 203.0.113.7, 198.51.100.2 ,"),
                ("RMS_WEBRTC_ICE_LITE", "false"),
            ]),
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c.webrtc.udp_port, 18443);
        assert_eq!(c.webrtc.public_ip, vec!["203.0.113.7", "198.51.100.2"]);
        assert!(!c.webrtc.ice_lite);
        c.normalize();
        assert!(c.validate().is_ok());
        c.webrtc.public_ip = vec!["not-an-ip".into()];
        assert!(c.validate().is_err());
        c.webrtc.public_ip.clear();
        c.webrtc.udp_port = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        let c = Config::from_toml_str(
            "[server]\nhttp_bind = \"127.0.0.1:9000\"\n[hls]\npart_duration = 0.33\n",
        )
        .unwrap();
        assert_eq!(c.server.http_bind, "127.0.0.1:9000");
        assert_eq!(c.server.max_upload_mb, 1024);
        assert_eq!(c.hls.part_duration, 0.33);
        assert_eq!(c.hls.window_segments, 6);
        assert_eq!(c.rtmp.app, "live");
    }

    #[test]
    fn example_config_parses_to_defaults() {
        let text = include_str!("../config.example.toml");
        let c = Config::from_toml_str(text).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn storage_section() {
        let c = Config::from_toml_str("[storage]\nmax_object_mb = 10\nroot = \"/srv/buckets\"\n")
            .unwrap();
        assert_eq!(c.storage.max_object_mb, 10);
        assert_eq!(c.storage_root(), PathBuf::from("/srv/buckets"));
        let mut w = Vec::new();
        let c = apply_env_overrides(
            Config::default(),
            vars(&[("RMS_STORAGE_MAX_OBJECT_MB", "64")]),
            &mut w,
        )
        .unwrap();
        assert_eq!(c.storage.max_object_mb, 64);
        assert!(w.is_empty());
        let mut bad = Config::default();
        bad.storage.max_object_mb = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn env_overrides_are_typed() {
        let mut warnings = Vec::new();
        let c = apply_env_overrides(
            Config::default(),
            vars(&[
                ("RMS_SERVER_HTTP_BIND", "127.0.0.1:18080"),
                ("RMS_SERVER_TRUST_PROXY", "true"),
                ("RMS_SERVER_MAX_UPLOAD_MB", "16"),
                ("RMS_HLS_PART_DURATION", "1"),
                ("RMS_SERVER_ADMIN_TOKEN", "12345"),
                ("RMS_LOG_LEVEL", "debug"),
                ("PATH", "/usr/bin"),
            ]),
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c.server.http_bind, "127.0.0.1:18080");
        assert!(c.server.trust_proxy);
        assert_eq!(c.server.max_upload_mb, 16);
        assert_eq!(c.hls.part_duration, 1.0);
        assert_eq!(c.server.admin_token, "12345");
        assert_eq!(c.log.level, "debug");
    }

    #[test]
    fn env_admin_token_alias_and_unknown_keys() {
        let mut warnings = Vec::new();
        let c = apply_env_overrides(
            Config::default(),
            vars(&[
                ("RMS_ADMIN_TOKEN", "secret"),
                ("RMS_NOPE_X", "1"),
                ("RMS_SERVER_NOPE", "1"),
            ]),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(c.server.admin_token, "secret");
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn env_override_bad_type_is_error() {
        let mut warnings = Vec::new();
        let r = apply_env_overrides(
            Config::default(),
            vars(&[("RMS_METRICS_RETENTION_DAYS", "many")]),
            &mut warnings,
        );
        assert!(r.is_err());
    }

    #[test]
    fn vod_defaults_and_ladder_validation() {
        let c = Config::default();
        assert!(c.vod.enabled);
        assert_eq!(c.vod.default_ladder, "standard");
        assert_eq!(c.vod.segment_duration, 4.0);
        assert_eq!(c.vod.encoder, "libx264");
        let names: Vec<&str> = c.vod.ladders.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["standard", "mobile", "single"]);
        assert_eq!(c.vod.ladder("standard").unwrap().renditions.len(), 4);
        assert!(c.validate().is_ok());
        assert_eq!(
            c.vod_inbox_dir(),
            PathBuf::from("./data").join("vod").join("inbox")
        );
        let mut custom = c.clone();
        custom.vod.inbox_dir = " /srv/inbox ".into();
        assert_eq!(custom.vod_inbox_dir(), PathBuf::from("/srv/inbox"));

        let invalid = |f: &dyn Fn(&mut VodConfig)| {
            let mut v = VodConfig::default();
            f(&mut v);
            v.validate().unwrap_err().to_string()
        };
        assert!(invalid(&|v| v.ladders.clear()).contains("at least one ladder"));
        assert!(invalid(&|v| v.default_ladder = "nope".into()).contains("default_ladder"));
        assert!(
            invalid(&|v| {
                let dup = v.ladders[1].clone();
                v.ladders.push(dup);
            })
            .contains("duplicate ladder name")
        );
        assert!(invalid(&|v| v.ladders[0].name = "a b".into()).contains("invalid ladder name"));
        assert!(invalid(&|v| v.ladders[0].name = "_failed".into()).contains("invalid ladder"));
        assert!(
            invalid(&|v| v.ladders[0].renditions.swap(0, 1)).contains("sorted by height"),
            "heights must be sorted"
        );
        assert!(
            invalid(&|v| v.ladders[0].renditions[1].height = 1080).contains("sorted by height"),
            "equal heights"
        );
        assert!(
            invalid(&|v| v.ladders[0].renditions[2].height = 0).contains("sorted by height"),
            "0 = source size only first"
        );
        assert!(
            invalid(&|v| v.ladders[0].renditions[3].name = "720p".into())
                .contains("duplicate rendition")
        );
        assert!(invalid(&|v| v.ladders[1].renditions.clear()).contains("at least one rendition"));
        assert!(invalid(&|v| v.ladders[0].renditions[0].audio_kbps = 0).contains("audio_kbps"));
        assert!(invalid(&|v| v.ladders[0].renditions[0].video_kbps = 10).contains("video_kbps"));
        assert!(invalid(&|v| v.segment_duration = 0.5).contains("segment_duration"));
        assert!(invalid(&|v| v.max_parallel_jobs = 0).contains("max_parallel_jobs"));
        assert!(invalid(&|v| v.encoder = " ".into()).contains("encoder"));

        // A TOML ladder replaces the built-in list; descriptions are optional.
        let c = Config::from_toml_str(
            "[vod]\ndefault_ladder = \"hd\"\n[[vod.ladders]]\nname = \"hd\"\nrenditions = [ { name = \"720p\", height = 720, video_kbps = 3000, audio_kbps = 128 } ]\n",
        )
        .unwrap();
        assert_eq!(c.vod.ladders.len(), 1);
        assert_eq!(c.vod.ladders[0].description, "");
        assert!(c.validate().is_ok());
        // Lists of tables cannot come from the environment.
        let mut warnings = Vec::new();
        let r = apply_env_overrides(
            Config::default(),
            vars(&[("RMS_VOD_LADDERS", "x")]),
            &mut warnings,
        );
        assert!(r.unwrap_err().to_string().contains("config file"));
        let c = apply_env_overrides(
            Config::default(),
            vars(&[
                ("RMS_VOD_ENCODER", "h264_nvenc"),
                ("RMS_VOD_MAX_PARALLEL_JOBS", "2"),
            ]),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(c.vod.encoder, "h264_nvenc");
        assert_eq!(c.vod.max_parallel_jobs, 2);
    }

    #[test]
    fn cluster_defaults_env_and_validation() {
        let c = Config::default();
        assert!(!c.cluster.enabled);
        assert_eq!(c.cluster.internal_bind, "0.0.0.0:8081");
        assert_eq!(
            (
                c.cluster.heartbeat_secs,
                c.cluster.node_ttl_secs,
                c.cluster.relay_idle_secs
            ),
            (5, 15, 30)
        );
        assert_eq!(c.webrtc.public_port, 0);
        assert_eq!(c.webrtc.advertised_port(), 8443);
        assert!(c.validate().is_ok(), "disabled cluster needs nothing");

        let mut warnings = Vec::new();
        let secret = "s".repeat(32);
        let mut c = apply_env_overrides(
            Config::default(),
            vars(&[
                ("RMS_CLUSTER_ENABLED", "true"),
                ("RMS_CLUSTER_NODE_ID", " n1 "),
                ("RMS_CLUSTER_DATABASE_URL", "postgres://rms:pw@db:5432/rms"),
                ("RMS_CLUSTER_ADVERTISE_URL", "http://rms1:8081/"),
                ("RMS_CLUSTER_INTERNAL_BIND", "127.0.0.1:18081"),
                ("RMS_CLUSTER_INTERNAL_SECRET", &secret),
                ("RMS_CLUSTER_HEARTBEAT_SECS", "2"),
                ("RMS_CLUSTER_NODE_TTL_SECS", "6"),
                ("RMS_ADMIN_TOKEN", "admin"),
                ("RMS_WEBRTC_PUBLIC_PORT", "8444"),
            ]),
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        c.normalize();
        assert!(c.validate().is_ok(), "{:?}", c.validate());
        assert_eq!(c.cluster.node_id, "n1");
        assert_eq!(c.cluster.advertise_url, "http://rms1:8081");
        assert_eq!(c.webrtc.advertised_port(), 8444);
        // Secrets never reach Debug output.
        let dbg = format!("{:?}", c.cluster);
        assert!(!dbg.contains("pw@") && !dbg.contains(&secret), "{dbg}");
        assert!(dbg.contains("postgres://rms:***@db:5432/rms"));

        let invalid = |f: &dyn Fn(&mut Config)| {
            let mut x = c.clone();
            f(&mut x);
            x.validate().unwrap_err().to_string()
        };
        assert!(invalid(&|x| x.cluster.database_url = "mysql://x".into()).contains("database_url"));
        assert!(invalid(&|x| x.cluster.database_url.clear()).contains("RMS_CLUSTER_DATABASE_URL"));
        assert!(invalid(&|x| x.cluster.advertise_url.clear()).contains("advertise_url"));
        assert!(invalid(&|x| x.cluster.internal_secret = "short".into()).contains("32"));
        assert!(invalid(&|x| x.server.admin_token.clear()).contains("admin_token"));
        assert!(invalid(&|x| x.cluster.node_id = "a b".into()).contains("node_id"));
        assert!(invalid(&|x| x.cluster.internal_bind = "nope".into()).contains("internal_bind"));
        assert!(invalid(&|x| x.cluster.node_ttl_secs = 3).contains("node_ttl_secs"));
        assert!(invalid(&|x| x.cluster.heartbeat_secs = 0).contains("heartbeat_secs"));

        // An empty node id defaults to the host name.
        let mut d = Config::default();
        d.normalize();
        assert_eq!(d.cluster.node_id, hostname());
        assert!(!d.cluster.node_id.is_empty());
        assert_eq!(
            redact_url_password("postgresql://u@h/db"),
            "postgresql://u@h/db"
        );
        assert_eq!(redact_url_password("garbage"), "garbage");
    }

    #[test]
    fn cli_wins() {
        let mut c = Config::default();
        c.apply_cli(&CliOverrides {
            data_dir: Some("/tmp/x".into()),
            http_bind: Some("127.0.0.1:1".into()),
            rtmp_bind: Some("127.0.0.1:2".into()),
        });
        assert_eq!(c.server.data_dir, PathBuf::from("/tmp/x"));
        assert_eq!(c.server.http_bind, "127.0.0.1:1");
        assert_eq!(c.rtmp.bind, "127.0.0.1:2");
        assert_eq!(c.rtmp_port(), 2);
    }
}
