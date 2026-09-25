//! Perintah yang berjalan tanpa membutuhkan config valid:
//! `init` / `config` / `doctor` / `completions`.

use crate::{
    cli::{Cli, ConfigCmd},
    config::{self, Config, Paths},
    docker::{port_free, Engine},
    exec,
    service::Registry,
    ui,
};
use anyhow::{bail, Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;
use std::{env, fs, io, path::Path, process::Command};

pub(super) fn init(paths: &Paths, force: bool) -> Result<()> {
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

pub(super) fn config(paths: &Paths, action: Option<ConfigCmd>) -> Result<()> {
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

pub(super) fn completions(shell: Shell) -> Result<()> {
    clap_complete::generate(shell, &mut Cli::command(), "dbx", &mut io::stdout());
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

pub(super) async fn doctor(paths: &Paths) -> Result<()> {
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
