//! Perintah yang bicara ke database lewat `docker exec`:
//! `sh` / `url` / `createdb` / `dump` / `restore`.

use super::{display_host, render_url, validate_db, Ctx};
use crate::{docker::Engine, exec, ui};
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, File},
    io::{self, IsTerminal},
    path::PathBuf,
    process::Stdio,
};

pub(super) async fn sh(ctx: &Ctx, service: &str) -> Result<()> {
    let s = ctx.reg.find(service)?;
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

pub(super) async fn url(ctx: &Ctx, service: &str, db: Option<String>) -> Result<()> {
    let s = ctx.reg.find(service)?;
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

pub(super) async fn createdb(ctx: &Ctx, service: &str, db: &str) -> Result<()> {
    validate_db(db)?;
    let s = ctx.reg.find(service)?;
    if s.client.create_db.is_empty() {
        bail!(
            "{} tidak punya client.create_db di {}",
            s.name,
            s.file.display()
        );
    }
    let container = ctx.engine().await?.require_running(s).await?;
    exec::run(&container, &exec::expand(&s.client.create_db, db))?;
    ui::ok(format!("database '{db}' dibuat di {}", s.name));
    Ok(())
}

pub(super) async fn dump(ctx: &Ctx, service: &str, db: &str, output: Option<String>) -> Result<()> {
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

pub(super) async fn restore(
    ctx: &Ctx,
    service: &str,
    db: &str,
    file: Option<PathBuf>,
) -> Result<()> {
    validate_db(db)?;
    let s = ctx.reg.find(service)?;
    if s.client.restore.is_empty() {
        bail!(
            "{} tidak punya client.restore di {}",
            s.name,
            s.file.display()
        );
    }
    let input = match file {
        Some(p) => {
            Stdio::from(File::open(&p).with_context(|| format!("gagal membuka {}", p.display()))?)
        }
        None if io::stdin().is_terminal() => {
            bail!(
                "sebutkan file, atau pipe lewat stdin: dbx restore {} {db} dump.sql",
                s.name
            )
        }
        None => Stdio::inherit(),
    };
    let container = ctx.engine().await?.require_running(s).await?;
    exec::pipe_in(&container, &exec::expand(&s.client.restore, db), input)?;
    ui::ok(format!("restore ke '{db}' selesai"));
    Ok(())
}
