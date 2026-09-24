//! Config global + lokasi folder.
//!
//! Struktur di ~/.config/dbx/ :
//!   config.toml            pengaturan global
//!   services/<nama>.toml   definisi tiap database (1 file = 1 service)

use anyhow::{bail, Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
};

const DEFAULT_CONFIG: &str = include_str!("../defaults/config.toml");
const DEFAULT_SERVICES: &[(&str, &str)] = &[
    (
        "postgres.toml",
        include_str!("../defaults/services/postgres.toml"),
    ),
    (
        "mariadb.toml",
        include_str!("../defaults/services/mariadb.toml"),
    ),
    (
        "redis.toml",
        include_str!("../defaults/services/redis.toml"),
    ),
    (
        "mongodb.toml",
        include_str!("../defaults/services/mongodb.toml"),
    ),
    (
        "elasticsearch.toml",
        include_str!("../defaults/services/elasticsearch.toml"),
    ),
];

pub const RESTART_POLICIES: &[&str] = &["no", "always", "unless-stopped", "on-failure"];

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub services_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn resolve(override_dir: Option<PathBuf>) -> Result<Self> {
        let pd =
            ProjectDirs::from("", "", "dbx").context("tidak bisa menentukan home directory")?;
        let config_dir = override_dir.unwrap_or_else(|| pd.config_dir().to_path_buf());
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            services_dir: config_dir.join("services"),
            data_dir: pd.data_dir().to_path_buf(),
            config_dir,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub container_prefix: String,
    pub network: String,
    pub bind_address: String,
    pub restart_policy: String,
    pub pull_missing: bool,
    pub auto_port: bool,
    pub wait_timeout_secs: u64,
    pub stop_timeout_secs: i32,
    pub default_services: Vec<String>,
    pub backup_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            container_prefix: "dbx".into(),
            network: "dbx".into(),
            bind_address: "127.0.0.1".into(),
            restart_policy: "unless-stopped".into(),
            pull_missing: true,
            auto_port: true,
            wait_timeout_secs: 90,
            stop_timeout_secs: 10,
            default_services: vec!["postgres".into(), "redis".into()],
            backup_dir: String::new(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let raw = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("gagal membaca {}", paths.config_file.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("config tidak valid: {}", paths.config_file.display()))?;
        cfg.validate()
            .with_context(|| format!("config tidak valid: {}", paths.config_file.display()))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.container_prefix.is_empty()
            || !self
                .container_prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("container_prefix hanya boleh huruf/angka/_/- dan tidak boleh kosong");
        }
        if self.bind_address.parse::<IpAddr>().is_err() {
            bail!("bind_address harus berupa IP, contoh \"127.0.0.1\"");
        }
        if !RESTART_POLICIES.contains(&self.restart_policy.as_str()) {
            bail!(
                "restart_policy '{}' tidak valid (pilih: {})",
                self.restart_policy,
                RESTART_POLICIES.join(" | ")
            );
        }
        if self.wait_timeout_secs == 0 {
            bail!("wait_timeout_secs harus > 0");
        }
        if self.stop_timeout_secs < 0 {
            bail!("stop_timeout_secs tidak boleh negatif");
        }
        Ok(())
    }

    /// Folder backup: config.backup_dir, atau ~/.local/share/dbx/backups.
    pub fn backup_dir(&self, paths: &Paths) -> PathBuf {
        let raw = self.backup_dir.trim();
        if raw.is_empty() {
            return paths.data_dir.join("backups");
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(b) = BaseDirs::new() {
                return b.home_dir().join(rest);
            }
        }
        PathBuf::from(raw)
    }
}

#[derive(Default)]
pub struct InitReport {
    pub created: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
}

fn write_default(path: &Path, content: &str, force: bool, rep: &mut InitReport) -> Result<()> {
    if path.exists() && !force {
        rep.skipped.push(path.to_path_buf());
        return Ok(());
    }
    fs::write(path, content).with_context(|| format!("gagal menulis {}", path.display()))?;
    rep.created.push(path.to_path_buf());
    Ok(())
}

/// Buat folder config + file default. File yang sudah ada tidak ditimpa kecuali `force`.
pub fn init(paths: &Paths, force: bool) -> Result<InitReport> {
    fs::create_dir_all(&paths.services_dir)
        .with_context(|| format!("gagal membuat {}", paths.services_dir.display()))?;
    let mut rep = InitReport::default();
    write_default(&paths.config_file, DEFAULT_CONFIG, force, &mut rep)?;
    for (file, content) in DEFAULT_SERVICES {
        write_default(&paths.services_dir.join(file), content, force, &mut rep)?;
    }
    Ok(rep)
}

/// Kalau config.toml belum ada → init otomatis. Return true kalau baru dibuat.
pub fn ensure_initialized(paths: &Paths) -> Result<bool> {
    if paths.config_file.exists() {
        return Ok(false);
    }
    init(paths, false)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Registry;
    use directories::BaseDirs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbx-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn paths_in(dir: PathBuf) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            services_dir: dir.join("services"),
            data_dir: dir.join("data"),
            config_dir: dir,
        }
    }

    #[test]
    fn defaults_parse_and_validate() {
        let p = paths_in(tmp("defaults"));
        let rep = init(&p, false).unwrap();
        assert_eq!(rep.created.len(), 1 + DEFAULT_SERVICES.len());
        Config::load(&p).unwrap();
        let reg = Registry::load(&p.services_dir).unwrap();
        assert_eq!(reg.services.len(), DEFAULT_SERVICES.len());
        assert_eq!(reg.find("pg").unwrap().name, "postgres");
        assert!(reg.find("mongo").is_err()); // disabled by default
                                             // init kedua tidak menimpa
        assert_eq!(init(&p, false).unwrap().created.len(), 0);
        let _ = fs::remove_dir_all(&p.config_dir);
    }

    #[test]
    fn typo_in_config_is_rejected() {
        let p = paths_in(tmp("typo"));
        init(&p, false).unwrap();
        fs::write(&p.config_file, "bind_adress = \"127.0.0.1\"\n").unwrap();
        assert!(Config::load(&p).is_err());
        fs::write(&p.config_file, "restart_policy = \"sometimes\"\n").unwrap();
        assert!(Config::load(&p).is_err());
        let _ = fs::remove_dir_all(&p.config_dir);
    }

    #[test]
    fn duplicate_alias_is_rejected() {
        let p = paths_in(tmp("dup"));
        init(&p, false).unwrap();
        let extra = fs::read_to_string(p.services_dir.join("postgres.toml"))
            .unwrap()
            .replace("name = \"postgres\"", "name = \"pg2\"");
        fs::write(p.services_dir.join("pg2.toml"), extra).unwrap(); // alias "pg" bentrok
        assert!(Registry::load(&p.services_dir).is_err());
        let _ = fs::remove_dir_all(&p.config_dir);
    }

    #[test]
    fn backup_dir_resolves_tilde_and_fallback() {
        let p = paths_in(tmp("backupdir"));

        // kosong → data_dir/backups
        let cfg = Config::default();
        assert_eq!(cfg.backup_dir(&p), p.data_dir.join("backups"));

        // "~/..." → home
        let cfg = Config {
            backup_dir: "~/dbx-backups".into(),
            ..Config::default()
        };
        let home = BaseDirs::new().unwrap().home_dir().to_path_buf();
        assert_eq!(cfg.backup_dir(&p), home.join("dbx-backups"));

        // path biasa → dipakai apa adanya
        let cfg = Config {
            backup_dir: "/tmp/dbx-backups".into(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_dir(&p), PathBuf::from("/tmp/dbx-backups"));

        let _ = fs::remove_dir_all(&p.config_dir);
    }

    #[test]
    fn negative_stop_timeout_is_rejected() {
        let p = paths_in(tmp("negtimeout"));
        init(&p, false).unwrap();
        fs::write(&p.config_file, "stop_timeout_secs = -1\n").unwrap();
        // {:#} → ikut rantai context, bukan cuma outer
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains("stop_timeout_secs"), "pesan: {err}");
        let _ = fs::remove_dir_all(&p.config_dir);
    }
}
