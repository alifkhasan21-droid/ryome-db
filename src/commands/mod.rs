//! Implementasi tiap subcommand.
//!
//! File ini hanya berisi dispatch dan konteks bersama; isi tiap perintah ada
//! di modul sebelah — `lifecycle` (up/down/restart/rm), `status` (ls/logs),
//! `client` (sh/url/createdb/dump/restore), dan `setup` (init/config/doctor).

mod client;
mod lifecycle;
mod setup;
mod status;

use crate::{
    cli::{Cli, Cmd},
    config::{self, Config, Paths},
    docker::Engine,
    service::{Registry, ServiceDef},
    ui,
};
use anyhow::{bail, Result};

/// Konteks yang dibutuhkan hampir semua perintah: lokasi folder, config
/// global, dan daftar service.
struct Ctx {
    paths: Paths,
    cfg: Config,
    reg: Registry,
}

impl Ctx {
    fn load(paths: Paths) -> Result<Self> {
        if config::ensure_initialized(&paths)? {
            ui::info(format!(
                "config awal dibuat di {} (edit sesuai kebutuhan: dbx config edit)",
                paths.config_dir.display()
            ));
        }
        let cfg = Config::load(&paths)?;
        let reg = Registry::load(&paths.services_dir)?;
        Ok(Self { paths, cfg, reg })
    }

    async fn engine(&self) -> Result<Engine> {
        let e = Engine::connect(self.cfg.clone())?;
        e.check_daemon().await?;
        Ok(e)
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let Cli { config_dir, cmd } = cli;
    let paths = Paths::resolve(config_dir)?;

    match cmd {
        // perintah yang tidak boleh gagal hanya karena config rusak
        Cmd::Init { force } => setup::init(&paths, force),
        Cmd::Config { action } => setup::config(&paths, action),
        Cmd::Doctor => setup::doctor(&paths).await,
        Cmd::Completions { shell } => setup::completions(shell),
        other => run_with_ctx(Ctx::load(paths)?, other).await,
    }
}

async fn run_with_ctx(ctx: Ctx, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Up {
            services,
            no_wait,
            recreate,
        } => lifecycle::up(&ctx, services, no_wait, recreate).await,

        Cmd::Down { services } => lifecycle::down(&ctx, services).await,

        Cmd::Restart { services } => lifecycle::restart(&ctx, services).await,

        Cmd::Rm {
            services,
            volumes,
            yes,
        } => lifecycle::rm(&ctx, services, volumes, yes).await,

        Cmd::Ls { all, json } => status::ls(&ctx, all, json).await,

        Cmd::Logs {
            service,
            follow,
            tail,
        } => status::logs(&ctx, &service, follow, &tail).await,

        Cmd::Sh { service, args } => client::sh(&ctx, &service, &args).await,

        Cmd::Url { service, db } => client::url(&ctx, &service, db).await,

        Cmd::Createdb { service, db } => client::createdb(&ctx, &service, &db).await,

        Cmd::Dump {
            service,
            db,
            output,
        } => client::dump(&ctx, &service, &db, output).await,

        Cmd::Restore { service, db, file } => client::restore(&ctx, &service, &db, file).await,

        // sudah ditangani di run()
        Cmd::Init { .. } | Cmd::Config { .. } | Cmd::Doctor | Cmd::Completions { .. } => Ok(()),
    }
}

// ───────────────────────── helpers ─────────────────────────

/// Host yang aman ditampilkan ke user — `0.0.0.0`/`::` bukan alamat yang
/// bisa dipakai untuk koneksi, jadi diganti loopback.
fn display_host(cfg: &Config) -> String {
    crate::docker::connect_host(&cfg.bind_address).into()
}

/// Isi `client.url` dengan host/port/db yang sebenarnya.
fn render_url(s: &ServiceDef, host: &str, port: u16, db: &str) -> String {
    s.client
        .url
        .replace("{host}", host)
        .replace("{port}", &port.to_string())
        .replace("{db}", db)
}

/// Nama database harus aman dipakai apa adanya di CLI container
/// (tanpa kutip) — tolak tanda baca, leading `-`, dan kepanjangan.
fn validate_db(db: &str) -> Result<()> {
    let ok = !db.is_empty()
        && db.len() <= 63
        && !db.starts_with('-')
        && db
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        bail!("nama database '{db}' tidak valid (huruf/angka/_/-, maks 63, tidak diawali '-')");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Client;
    use std::path::PathBuf;

    fn pg() -> ServiceDef {
        ServiceDef {
            name: "postgres".into(),
            aliases: vec!["pg".into()],
            enabled: true,
            image: "postgres:17".into(),
            port: 5432,
            container_port: 5432,
            data_path: "/var/lib/postgresql/data".into(),
            cmd: vec![],
            env: Default::default(),
            healthcheck: None,
            client: Client {
                default_db: "postgres".into(),
                url: "postgresql://postgres:postgres@{host}:{port}/{db}".into(),
                ..Default::default()
            },
            file: PathBuf::new(),
        }
    }

    #[test]
    fn render_url_fills_placeholders() {
        assert_eq!(
            render_url(&pg(), "127.0.0.1", 5432, "myapp"),
            "postgresql://postgres:postgres@127.0.0.1:5432/myapp"
        );
    }

    #[test]
    fn display_host_never_prints_wildcard_bind() {
        let mut cfg = Config::default();
        assert_eq!(display_host(&cfg), "127.0.0.1");
        cfg.bind_address = "0.0.0.0".into();
        assert_eq!(display_host(&cfg), "127.0.0.1", "0.0.0.0 → 127.0.0.1");
        cfg.bind_address = "::".into();
        assert_eq!(display_host(&cfg), "127.0.0.1");
        cfg.bind_address = "192.168.1.5".into();
        assert_eq!(
            display_host(&cfg),
            "192.168.1.5",
            "IP spesifik dipertahankan"
        );
    }

    #[test]
    fn validate_db_rejects_injection_and_overlong_names() {
        assert!(validate_db("myapp").is_ok());
        assert!(validate_db("my-app_2").is_ok());

        assert!(validate_db("").is_err());
        assert!(validate_db("-leading").is_err(), "awalan - merusak arg CLI");
        assert!(validate_db("a\"; DROP TABLE x;--").is_err());
        assert!(validate_db(&"a".repeat(64)).is_err(), "maks 63");
        assert!(validate_db(&"a".repeat(63)).is_ok());
    }
}
