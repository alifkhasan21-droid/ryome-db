//! Perintah lifecycle container: `up` / `down` / `restart` / `rm`.

use super::{display_host, render_url, Ctx};
use crate::{
    docker::{Engine, StopResult, UpResult},
    service::ServiceDef,
    ui,
};
use anyhow::{bail, Result};
use std::{
    io::{self, IsTerminal},
    time::{Duration, Instant},
};

pub(super) async fn up(
    ctx: &Ctx,
    services: Vec<String>,
    no_wait: bool,
    recreate: bool,
) -> Result<()> {
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
    bring_up(ctx, &engine, &defs, !no_wait, recreate).await
}

pub(super) async fn down(ctx: &Ctx, services: Vec<String>) -> Result<()> {
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
            StopResult::NotCreated if !all => ui::info(format!("{} belum pernah dibuat", s.name)),
            _ => {}
        }
    }
    if all && stopped == 0 {
        ui::info("tidak ada database yang sedang jalan");
    }
    Ok(())
}

pub(super) async fn restart(ctx: &Ctx, services: Vec<String>) -> Result<()> {
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
    bring_up(ctx, &engine, &defs, true, false).await
}

pub(super) async fn rm(ctx: &Ctx, services: Vec<String>, volumes: bool, yes: bool) -> Result<()> {
    let defs = ctx.reg.find_many(&services)?;
    if volumes && !yes {
        if !io::stdin().is_terminal() {
            bail!("--volumes menghapus data permanen; pakai --yes untuk mode non-interaktif");
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
