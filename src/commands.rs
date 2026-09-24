//! Implementasi tiap subcommand.

use crate::{
    cli::{Cli, Cmd, ConfigCmd},
    config::{self, Config, Paths},
    docker::{port_free, Engine, Status, StopResult, UpResult},
    exec,
    service::{Registry, ServiceDef},
    ui,
};
use anyhow::{bail, Context, Result};
use clap::CommandFactory;
use std::{
    env,
    fs::{self, File},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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
        Cmd::Init { force } => cmd_init(&paths, force),
        Cmd::Config { action } => cmd_config(&paths, action),
        Cmd::Doctor => cmd_doctor(&paths).await,
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "dbx", &mut io::stdout());
            Ok(())
        }
        other => run_with_ctx(Ctx::load(paths)?, other).await,
    }
}

async fn run_with_ctx(ctx: Ctx, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Up {
            services,
            no_wait,
            recreate,
        } => {
            let names = if services.is_empty() {
                ctx.cfg.default_services.clone()
            } else {
                services
            };
            if names.is_empty() {
                bail!(
                    "tidak ada service. Sebut namanya (dbx up pg) atau isi default_services di {}",
                    ctx.paths.config_file.display()
                );
            }
            let defs = ctx.reg.find_many(&names)?;
            let engine = ctx.engine().await?;
            bring_up(&ctx, &engine, &defs, !no_wait, recreate).await
        }

        Cmd::Down { services } => {
            let all = services.is_empty();
            let defs: Vec<&ServiceDef> = if all {
                ctx.reg.enabled().collect()
            } else {
                ctx.reg.find_many(&services)?
            };
            let engine = ctx.engine().await?;
            let mut stopped = 0;
            for s in defs {
                match engine.stop(s).await? {
                    StopResult::Stopped => {
                        stopped += 1;
                        ui::ok(format!("{} berhenti", s.name));
                    }
                    StopResult::AlreadyStopped if !all => {
                        ui::info(format!("{} memang tidak jalan", s.name))
                    }
                    StopResult::NotCreated if !all => {
                        ui::info(format!("{} belum pernah dibuat", s.name))
                    }
                    _ => {}
                }
            }
            if all && stopped == 0 {
                ui::info("tidak ada database yang sedang jalan");
            }
            Ok(())
        }

        Cmd::Restart { services } => {
            let names = if services.is_empty() {
                ctx.cfg.default_services.clone()
            } else {
                services
            };
            let defs = ctx.reg.find_many(&names)?;
            let engine = ctx.engine().await?;
            for s in &defs {
                engine.stop(s).await?;
            }
            bring_up(&ctx, &engine, &defs, true, false).await
        }

        Cmd::Rm {
            services,
            volumes,
            yes,
        } => {
            let defs = ctx.reg.find_many(&services)?;
            if volumes && !yes {
                if !io::stdin().is_terminal() {
                    bail!(
                        "--volumes menghapus data permanen; pakai --yes untuk mode non-interaktif"
                    );
                }
                let names: Vec<&str> = defs.iter().map(|s| s.name.as_str()).collect();
                eprint!(
                    "Hapus container + VOLUME data untuk {}? Data hilang permanen. [y/N] ",
                    names.join(", ")
                );
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
                    bail!("dibatalkan");
                }
            }
            let engine = ctx.engine().await?;
            for s in defs {
                engine.remove(s, volumes).await?;
                ui::ok(format!(
                    "{} dihapus{}",
                    s.name,
                    if volumes {
                        " (beserta volume)"
                    } else {
                        " (volume data dipertahankan)"
                    }
                ));
            }
            Ok(())
        }

        Cmd::Ls { all, json } => cmd_ls(&ctx, all, json).await,

        Cmd::Logs {
            service,
            follow,
            tail,
        } => {
            let s = ctx.reg.find(&service)?;
            ctx.engine().await?.logs(s, follow, &tail).await
        }

        Cmd::Sh { service } => {
            let s = ctx.reg.find(&service)?;
            if s.client.shell.is_empty() {
                bail!(
                    "{} tidak punya client.shell di {}",
                    s.name,
                    s.file.display()
                );
            }
            let container = ctx.engine().await?.require_running(s).await?;
            exec::interactive(
                &container,
                &exec::expand(&s.client.shell, &s.client.default_db),
            )
        }

        Cmd::Url { service, db } => {
            let s = ctx.reg.find(&service)?;
            if s.client.url.is_empty() {
                bail!("{} tidak punya client.url di {}", s.name, s.file.display());
            }
            // port asli (bisa beda kalau auto_port); kalau docker mati pakai port config
            let mut port = s.port;
            if let Ok(engine) = Engine::connect(ctx.cfg.clone()) {
                if let Some(p) = engine.status(s).await.ok().and_then(|st| st.port) {
                    port = p;
                }
            }
            let db = db.unwrap_or_else(|| s.client.default_db.clone());
            println!("{}", render_url(s, &display_host(&ctx.cfg), port, &db));
            Ok(())
        }

        Cmd::Createdb { service, db } => {
            validate_db(&db)?;
            let s = ctx.reg.find(&service)?;
            if s.client.create_db.is_empty() {
                bail!(
                    "{} tidak punya client.create_db di {}",
                    s.name,
                    s.file.display()
                );
            }
            let container = ctx.engine().await?.require_running(s).await?;
            exec::run(&container, &exec::expand(&s.client.create_db, &db))?;
            ui::ok(format!("database '{db}' dibuat di {}", s.name));
            Ok(())
        }

        Cmd::Dump {
            service,
            db,
            output,
        } => cmd_dump(&ctx, &service, &db, output).await,

        Cmd::Restore { service, db, file } => {
            validate_db(&db)?;
            let s = ctx.reg.find(&service)?;
            if s.client.restore.is_empty() {
                bail!(
                    "{} tidak punya client.restore di {}",
                    s.name,
                    s.file.display()
                );
            }
            let input = match &file {
                Some(p) => Stdio::from(
                    File::open(p).with_context(|| format!("gagal membuka {}", p.display()))?,
                ),
                None if io::stdin().is_terminal() => {
                    bail!(
                        "sebutkan file, atau pipe lewat stdin: dbx restore {} {db} dump.sql",
                        s.name
                    )
                }
                None => Stdio::inherit(),
            };
            let container = ctx.engine().await?.require_running(s).await?;
            exec::pipe_in(&container, &exec::expand(&s.client.restore, &db), input)?;
            ui::ok(format!("restore ke '{db}' selesai"));
            Ok(())
        }

        // sudah ditangani di run()
        Cmd::Init { .. } | Cmd::Config { .. } | Cmd::Doctor | Cmd::Completions { .. } => Ok(()),
    }
}

// ───────────────────────── helpers ─────────────────────────

fn display_host(cfg: &Config) -> String {
    crate::docker::connect_host(&cfg.bind_address).into()
}

fn render_url(s: &ServiceDef, host: &str, port: u16, db: &str) -> String {
    s.client
        .url
        .replace("{host}", host)
        .replace("{port}", &port.to_string())
        .replace("{db}", db)
}

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

/// Start semua service paralel (pull & create tidak saling menunggu), lalu
/// tunggu sehatnya juga paralel — total waktu = max, bukan jumlah.
///
/// Output tetap dicetak berurutan sesuai `defs` supaya tidak berantakan.
async fn bring_up(
    ctx: &Ctx,
    engine: &Engine,
    defs: &[&ServiceDef],
    wait: bool,
    recreate: bool,
) -> Result<()> {
    let mut failed = 0;

    let ups =
        futures_util::future::join_all(defs.iter().copied().map(|s| engine.up(s, recreate))).await;
    let mut started: Vec<(&ServiceDef, UpResult)> = Vec::new();
    for (s, res) in defs.iter().copied().zip(ups) {
        match res {
            Ok(r) => started.push((s, r)),
            Err(e) => {
                ui::fail(format!("{}: {e:#}", s.name));
                failed += 1;
            }
        }
    }

    if wait && started.iter().any(|(_, r)| *r != UpResult::AlreadyRunning) {
        ui::info("menunggu database siap…");
    }

    // (hasil wait, lama tunggu service ini)
    let waits: Vec<(Result<()>, Duration)> = if wait {
        futures_util::future::join_all(started.iter().map(|(s, _)| {
            let t = Instant::now();
            async move { (engine.wait_ready(s).await, t.elapsed()) }
        }))
        .await
    } else {
        (0..started.len())
            .map(|_| (Ok(()), Duration::ZERO))
            .collect()
    };

    for ((s, r), (w, elapsed)) in started.iter().zip(waits) {
        if let Err(e) = w {
            ui::fail(format!("{e:#}"));
            failed += 1;
            continue;
        }
        let port = engine
            .status(s)
            .await
            .ok()
            .and_then(|st| st.port)
            .unwrap_or(s.port);
        let verb = match r {
            UpResult::Created => "dibuat & jalan",
            UpResult::Started => "jalan",
            UpResult::AlreadyRunning => "sudah jalan",
        };
        let host = display_host(&ctx.cfg);
        ui::ok(format!(
            "{:<14} {verb} di {host}:{port}{}",
            s.name,
            if wait && *r != UpResult::AlreadyRunning {
                format!(" ({:.1}s)", elapsed.as_secs_f32())
            } else {
                String::new()
            }
        ));
        if !s.client.url.is_empty() {
            eprintln!(
                "    {}",
                ui::dim(&render_url(s, &host, port, &s.client.default_db))
            );
        }
    }

    if failed > 0 {
        bail!("{failed} service bermasalah");
    }
    Ok(())
}

async fn cmd_ls(ctx: &Ctx, all: bool, json: bool) -> Result<()> {
    let defs: Vec<&ServiceDef> = if all {
        ctx.reg.services.iter().collect()
    } else {
        ctx.reg.enabled().collect()
    };
    if defs.is_empty() {
        ui::info(format!(
            "belum ada service. Tambah file .toml di {}",
            ctx.paths.services_dir.display()
        ));
        return Ok(());
    }
    // Daemon mati bukan alasan untuk menyembunyikan daftar service:
    // tetap tampilkan apa yang diketahui dari config.
    let engine = Engine::connect(ctx.cfg.clone())?;
    let daemon_up = engine.check_daemon().await;
    if let Err(e) = &daemon_up {
        ui::warn("docker daemon tidak bisa diakses — status di bawah hanya dari config");
        // rincian berisi saran perbaikan dan bisa multi-baris, jangan dibungkus
        for line in e.to_string().lines() {
            ui::info(line);
        }
    }
    let mut rows: Vec<Status> = Vec::new();
    for s in defs {
        match &daemon_up {
            Ok(()) => rows.push(engine.status(s).await?),
            Err(_) => rows.push(Status {
                service: s.name.clone(),
                container: engine.container_name(s),
                enabled: s.enabled,
                state: "unknown".into(),
                health: "-".into(),
                port: None,
                image: s.image.clone(),
            }),
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.service.clone(),
                if r.enabled {
                    r.state.clone()
                } else {
                    "disabled".into()
                },
                r.health.clone(),
                r.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                r.image.clone(),
            ]
        })
        .collect();
    let header = ["SERVICE", "STATE", "HEALTH", "PORT", "IMAGE"];
    let mut w = header.map(|h| h.len());
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }

    let line: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{h:<w$}", w = w[i]))
        .collect();
    println!("{}", ui::dim(line.join("  ").trim_end()));

    for row in &cells {
        let pad = |i: usize| format!("{:<w$}", row[i], w = w[i]);
        let state = match row[1].as_str() {
            "running" => ui::green(&pad(1)),
            "absent" | "created" | "disabled" => ui::dim(&pad(1)),
            "exited" | "dead" => ui::red(&pad(1)),
            _ => ui::yellow(&pad(1)), // starting/restarting/unknown
        };
        let health = match row[2].as_str() {
            "healthy" => ui::green(&pad(2)),
            "unhealthy" => ui::red(&pad(2)),
            "starting" => ui::yellow(&pad(2)),
            _ => ui::dim(&pad(2)),
        };
        println!(
            "{}  {}  {}  {}  {}",
            ui::bold(&pad(0)),
            state,
            health,
            pad(3),
            row[4]
        );
    }
    Ok(())
}

async fn cmd_dump(ctx: &Ctx, service: &str, db: &str, output: Option<String>) -> Result<()> {
    validate_db(db)?;
    let s = ctx.reg.find(service)?;
    if s.client.dump.is_empty() {
        bail!("{} tidak punya client.dump di {}", s.name, s.file.display());
    }
    let container = ctx.engine().await?.require_running(s).await?;
    let args = exec::expand(&s.client.dump, db);

    if output.as_deref() == Some("-") {
        return exec::pipe_out(&container, &args, Stdio::inherit());
    }

    let path = match output {
        Some(p) => PathBuf::from(p),
        None => {
            let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
            ctx.cfg
                .backup_dir(&ctx.paths)
                .join(&s.name)
                .join(format!("{db}-{ts}.{}", s.client.ext()))
        }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("gagal membuat {}", dir.display()))?;
    }
    let file = File::create(&path).with_context(|| format!("gagal membuat {}", path.display()))?;
    if let Err(e) = exec::pipe_out(&container, &args, Stdio::from(file)) {
        let _ = fs::remove_file(&path); // jangan tinggalkan dump setengah jadi
        return Err(e);
    }
    let size = fs::metadata(&path)?.len();
    ui::ok(format!(
        "dump '{db}' → {} ({})",
        path.display(),
        ui::human_size(size)
    ));
    println!("{}", path.display()); // stdout: path, biar bisa dipakai di script
    Ok(())
}

fn cmd_init(paths: &Paths, force: bool) -> Result<()> {
    let rep = config::init(paths, force)?;
    for p in &rep.created {
        ui::ok(format!("dibuat  {}", p.display()));
    }
    for p in &rep.skipped {
        ui::info(format!(
            "dilewati {} (sudah ada, pakai --force untuk menimpa)",
            p.display()
        ));
    }
    ui::info(format!(
        "edit pengaturan: dbx config edit  |  folder: {}",
        paths.config_dir.display()
    ));
    Ok(())
}

fn open_editor(path: &Path) -> Result<()> {
    let ed = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    let mut parts = ed.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    let st = Command::new(bin)
        .args(parts)
        .arg(path)
        .status()
        .with_context(|| format!("gagal menjalankan editor '{ed}' (set $EDITOR)"))?;
    if !st.success() {
        bail!("editor keluar dengan status {st}");
    }
    Ok(())
}

fn cmd_config(paths: &Paths, action: Option<ConfigCmd>) -> Result<()> {
    match action {
        Some(ConfigCmd::Path) => {
            println!("{}", paths.config_dir.display());
            Ok(())
        }
        Some(ConfigCmd::Show) => {
            config::ensure_initialized(paths)?;
            print!(
                "{}",
                fs::read_to_string(&paths.config_file)
                    .with_context(|| format!("gagal membaca {}", paths.config_file.display()))?
            );
            Ok(())
        }
        Some(ConfigCmd::Edit { service }) => {
            config::ensure_initialized(paths)?;
            let target = match service {
                None => paths.config_file.clone(),
                Some(name) => {
                    let from_reg = Registry::load(&paths.services_dir)
                        .ok()
                        .and_then(|r| r.find_any(&name).map(|s| s.file.clone()));
                    match from_reg {
                        Some(f) => f,
                        None => {
                            let p = paths.services_dir.join(format!("{name}.toml"));
                            if !p.exists() {
                                bail!(
                                    "service '{name}' tidak ditemukan di {}",
                                    paths.services_dir.display()
                                );
                            }
                            p
                        }
                    }
                }
            };
            open_editor(&target)?;
            // validasi setelah edit supaya typo ketahuan langsung
            match Config::load(paths).and_then(|_| Registry::load(&paths.services_dir).map(|_| ()))
            {
                Ok(()) => ui::ok("config valid"),
                Err(e) => ui::warn(format!("config bermasalah: {e:#}")),
            }
            Ok(())
        }
        None => {
            let cfg = Config::load(paths).unwrap_or_default();
            println!("{:<10} {}", ui::bold("folder"), paths.config_dir.display());
            println!("{:<10} {}", ui::bold("config"), paths.config_file.display());
            println!(
                "{:<10} {}",
                ui::bold("services"),
                paths.services_dir.display()
            );
            println!(
                "{:<10} {}",
                ui::bold("backups"),
                cfg.backup_dir(paths).display()
            );
            match Registry::load(&paths.services_dir) {
                Ok(r) => {
                    for s in &r.services {
                        let file = s
                            .file
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let tag = if s.enabled { "aktif" } else { "off" };
                        println!("  {:<16} {:<6} {}", s.name, tag, ui::dim(&file));
                    }
                }
                Err(_) => ui::info("belum ada config — jalankan: dbx init"),
            }
            Ok(())
        }
    }
}

async fn cmd_doctor(paths: &Paths) -> Result<()> {
    let mut bad = 0;

    // Config belum ada = satu masalah, bukan tiga (config + services + …)
    // — makanya load-nya tidak dijalankan kalau file-nya memang tidak ada.
    let has_config = paths.config_file.exists();
    if has_config {
        ui::ok(format!("config dir: {}", paths.config_dir.display()));
    } else {
        ui::warn("belum ada config — jalankan: dbx init");
        bad += 1;
    }

    let cfg = if has_config {
        match Config::load(paths) {
            Ok(c) => {
                ui::ok("config.toml valid");
                Some(c)
            }
            Err(e) => {
                ui::fail(format!("{e:#}"));
                bad += 1;
                None
            }
        }
    } else {
        None
    };

    let reg = if has_config {
        match Registry::load(&paths.services_dir) {
            Ok(r) => {
                ui::ok(format!(
                    "{} service ({} aktif)",
                    r.services.len(),
                    r.enabled().count()
                ));
                Some(r)
            }
            Err(e) => {
                ui::fail(format!("{e:#}"));
                bad += 1;
                None
            }
        }
    } else {
        None
    };

    match exec::docker_version() {
        Ok(v) => ui::ok(v),
        Err(e) => {
            ui::fail(format!("{e:#}"));
            bad += 1;
        }
    }

    // Cek daemon tetap dijalankan walau config rusak/belum ada — itu justru
    // bagian terpenting doctor. Config default cukup untuk sekadar ping.
    let mut engine = None;
    match Engine::connect(cfg.clone().unwrap_or_default()) {
        Ok(e) => match e.check_daemon().await {
            Ok(()) => {
                ui::ok("docker daemon bisa diakses");
                engine = Some(e);
            }
            Err(err) => {
                ui::fail(format!("{err:#}"));
                bad += 1;
            }
        },
        Err(err) => {
            ui::fail(format!("{err:#}"));
            bad += 1;
        }
    }

    if let (Some(cfg), Some(reg)) = (&cfg, &reg) {
        // typo di default_services baru ketahuan saat `dbx up` — cek di sini
        if cfg.default_services.is_empty() {
            ui::warn(
                "default_services kosong — `dbx up` tanpa argumen tidak akan menjalankan apa-apa",
            );
        } else {
            match reg.find_many(&cfg.default_services) {
                Ok(list) => {
                    let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
                    ui::ok(format!("default_services: {}", names.join(", ")));
                }
                Err(e) => {
                    ui::fail(format!("default_services: {e:#}"));
                    bad += 1;
                }
            }
        }

        for s in reg.enabled() {
            let running = match &engine {
                Some(e) => e
                    .status(s)
                    .await
                    .map(|st| st.state == "running")
                    .unwrap_or(false),
                None => false,
            };
            if running {
                ui::ok(format!("{}: jalan", s.name));
            } else if port_free(&cfg.bind_address, s.port) {
                ui::ok(format!("{}: port {} bebas", s.name, s.port));
            } else if cfg.auto_port {
                ui::warn(format!(
                    "{}: port {} dipakai proses lain (auto_port akan pilih port lain)",
                    s.name, s.port
                ));
            } else {
                ui::fail(format!("{}: port {} dipakai proses lain", s.name, s.port));
                bad += 1;
            }
        }
    }

    if bad > 0 {
        bail!("{bad} masalah ditemukan");
    }
    ui::ok("semua beres");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Client;

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
