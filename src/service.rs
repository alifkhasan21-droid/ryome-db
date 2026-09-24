//! Definisi service (1 file TOML di services/ = 1 database).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn yes() -> bool {
    true
}
fn secs3() -> u64 {
    3
}
fn retries20() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,

    pub image: String,
    /// Port di host.
    pub port: u16,
    /// Port di dalam container.
    pub container_port: u16,
    /// Path data di dalam container (di-mount ke named volume).
    pub data_path: String,
    /// Override command container (opsional).
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub client: Client,

    /// Diisi saat load, bukan dari file.
    #[serde(skip)]
    pub file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    pub cmd: Vec<String>,
    #[serde(default = "secs3")]
    pub interval_secs: u64,
    #[serde(default = "secs3")]
    pub timeout_secs: u64,
    #[serde(default = "retries20")]
    pub retries: u32,
    #[serde(default)]
    pub start_period_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Client {
    pub default_db: String,
    /// Template URL: {host} {port} {db}
    pub url: String,
    /// Command interaktif (`dbx sh`). {db} = default_db
    pub shell: Vec<String>,
    pub create_db: Vec<String>,
    /// Harus menulis dump ke stdout. {db} = nama database
    pub dump: Vec<String>,
    /// Membaca dump dari stdin. {db} = nama database
    pub restore: Vec<String>,
    /// Ekstensi file dump (default "sql")
    pub dump_ext: String,
}

impl Client {
    pub fn ext(&self) -> &str {
        if self.dump_ext.is_empty() {
            "sql"
        } else {
            &self.dump_ext
        }
    }
}

pub struct Registry {
    pub services: Vec<ServiceDef>,
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl Registry {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .with_context(|| format!("gagal membaca folder {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort();

        let mut services: Vec<ServiceDef> = Vec::new();
        let mut seen: Vec<(String, PathBuf)> = Vec::new();

        for file in files {
            let raw = fs::read_to_string(&file)
                .with_context(|| format!("gagal membaca {}", file.display()))?;
            let mut s: ServiceDef = toml::from_str(&raw)
                .with_context(|| format!("service tidak valid: {}", file.display()))?;
            s.file = file.clone();

            if !valid_ident(&s.name) {
                bail!("{}: name harus huruf kecil/angka/_/-", file.display());
            }
            if let Some(h) = &s.healthcheck {
                if h.cmd.is_empty() {
                    bail!("{}: healthcheck.cmd tidak boleh kosong", file.display());
                }
            }
            for key in std::iter::once(&s.name).chain(s.aliases.iter()) {
                let k = key.to_lowercase();
                if let Some((_, other)) = seen.iter().find(|(n, _)| *n == k) {
                    bail!(
                        "nama/alias '{}' bentrok antara {} dan {}",
                        key,
                        other.display(),
                        file.display()
                    );
                }
                seen.push((k, file.clone()));
            }
            services.push(s);
        }
        Ok(Self { services })
    }

    pub fn enabled(&self) -> impl Iterator<Item = &ServiceDef> {
        self.services.iter().filter(|s| s.enabled)
    }

    /// Cari service (termasuk yang disabled) berdasarkan nama/alias.
    pub fn find_any(&self, key: &str) -> Option<&ServiceDef> {
        let k = key.to_lowercase();
        self.services
            .iter()
            .find(|s| s.name.to_lowercase() == k || s.aliases.iter().any(|a| a.to_lowercase() == k))
    }

    pub fn find(&self, key: &str) -> Result<&ServiceDef> {
        match self.find_any(key) {
            Some(s) if s.enabled => Ok(s),
            Some(s) => bail!(
                "service '{}' dinonaktifkan — set `enabled = true` di {}",
                s.name,
                s.file.display()
            ),
            None => {
                let names: Vec<&str> = self.enabled().map(|s| s.name.as_str()).collect();
                bail!(
                    "service '{key}' tidak dikenal. Tersedia: {}",
                    if names.is_empty() {
                        "(kosong)".into()
                    } else {
                        names.join(", ")
                    }
                )
            }
        }
    }

    /// Resolve banyak nama sekaligus, buang duplikat, urutan dipertahankan.
    pub fn find_many(&self, keys: &[String]) -> Result<Vec<&ServiceDef>> {
        let mut out: Vec<&ServiceDef> = Vec::new();
        for k in keys {
            let s = self.find(k)?;
            if !out.iter().any(|x| x.name == s.name) {
                out.push(s);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_falls_back_to_sql() {
        let c = Client::default();
        assert_eq!(c.ext(), "sql");
        let c = Client {
            dump_ext: "archive".into(),
            ..Default::default()
        };
        assert_eq!(c.ext(), "archive");
    }

    #[test]
    fn valid_ident_rules() {
        assert!(valid_ident("postgres"));
        assert!(valid_ident("my-db_1"));
        assert!(!valid_ident(""));
        assert!(!valid_ident("Postgres")); // huruf besar ditolak
        assert!(!valid_ident("my db"));
    }

    #[test]
    fn find_is_case_insensitive_and_honours_aliases() {
        let dir = std::env::temp_dir().join(format!("dbx-test-reg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("postgres.toml"),
            r#"
name = "postgres"
aliases = ["PG"]
image = "postgres:17"
port = 5432
container_port = 5432
data_path = "/var/lib/postgresql/data"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("redis.toml"),
            r#"
name = "redis"
enabled = false
image = "redis:7"
port = 6379
container_port = 6379
data_path = "/data"
"#,
        )
        .unwrap();

        let reg = Registry::load(&dir).unwrap();
        assert_eq!(reg.find("PG").unwrap().name, "postgres");
        assert_eq!(reg.find("postgres").unwrap().name, "postgres");
        // disabled: ketemu lewat find_any, tapi find menolak dengan pesan jelas
        assert_eq!(reg.find_any("redis").unwrap().name, "redis");
        let err = format!("{:#}", reg.find("redis").unwrap_err());
        assert!(err.contains("dinonaktifkan"), "pesan: {err}");
        let err = format!("{:#}", reg.find("mysql").unwrap_err());
        assert!(err.contains("tidak dikenal"), "pesan: {err}");

        let many = reg.find_many(&["pg".into(), "postgres".into()]).unwrap();
        assert_eq!(many.len(), 1, "duplikat harus dibuang");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
