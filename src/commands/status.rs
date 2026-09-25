//! Perintah inspeksi: `ls` dan `logs`.

use super::Ctx;
use crate::{
    docker::{Engine, Status},
    service::ServiceDef,
    ui,
};
use anyhow::Result;

pub(super) async fn ls(ctx: &Ctx, all: bool, json: bool) -> Result<()> {
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

pub(super) async fn logs(ctx: &Ctx, service: &str, follow: bool, tail: &str) -> Result<()> {
    let s = ctx.reg.find(service)?;
    ctx.engine().await?.logs(s, follow, tail).await
}
