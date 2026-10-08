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
