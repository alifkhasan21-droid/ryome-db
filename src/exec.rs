//! Eksekusi command di dalam container lewat `docker exec` (butuh TTY/stdin asli,
//! jadi pakai CLI docker, bukan API).

use anyhow::{anyhow, bail, Result};
use std::{
    io::{self, IsTerminal},
    process::{Command, Stdio},
};

fn docker() -> Command {
    Command::new("docker")
}

fn spawn_err(e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::NotFound {
        anyhow!("CLI `docker` tidak ditemukan di PATH (Arch: sudo pacman -S docker)")
    } else {
        e.into()
    }
}

/// Ganti {db} di argumen command.
pub fn expand(args: &[String], db: &str) -> Vec<String> {
    args.iter().map(|a| a.replace("{db}", db)).collect()
}

/// Sama seperti `docker --version` (dipakai doctor).
pub fn docker_version() -> Result<String> {
    let out = docker().arg("--version").output().map_err(spawn_err)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Shell/client interaktif. Kalau stdin bukan TTY (pipe), tetap jalan tanpa -t.
pub fn interactive(container: &str, args: &[String]) -> Result<()> {
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    let st = docker()
        .args(["exec", if tty { "-it" } else { "-i" }, container])
        .args(args)
        .status()
        .map_err(spawn_err)?;
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
    Ok(())
}

/// Jalankan command biasa (tanpa stdin).
pub fn run(container: &str, args: &[String]) -> Result<()> {
    let st = docker()
        .args(["exec", container])
        .args(args)
        .status()
        .map_err(spawn_err)?;
    if !st.success() {
        bail!("command gagal (exit {})", st.code().unwrap_or(-1));
    }
    Ok(())
}

/// stdout command → `out` (file atau terminal).
pub fn pipe_out(container: &str, args: &[String], out: Stdio) -> Result<()> {
    let st = docker()
        .args(["exec", container])
        .args(args)
        .stdout(out)
        .status()
        .map_err(spawn_err)?;

    // Pembaca sudah menutup pipe (`dbx dump pg db -o - | head -5`): dump-nya
    // sendiri tidak gagal, jangan dilaporkan sebagai error.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if st.signal() == Some(libc::SIGPIPE) {
            return Ok(());
        }
    }

    if !st.success() {
        bail!("dump gagal (exit {})", st.code().unwrap_or(-1));
    }
    Ok(())
}

/// `input` (file atau stdin) → stdin command.
pub fn pipe_in(container: &str, args: &[String], input: Stdio) -> Result<()> {
    let st = docker()
        .args(["exec", "-i", container])
        .args(args)
        .stdin(input)
        .status()
        .map_err(spawn_err)?;
    if !st.success() {
        bail!("restore gagal (exit {})", st.code().unwrap_or(-1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn expand_replaces_db_placeholder() {
        let args = vec![
            "psql".to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            "-c".to_string(),
            "CREATE DATABASE \"{db}\"".to_string(),
        ];
        let out = expand(&args, "myapp");
        assert_eq!(out[4], "CREATE DATABASE \"myapp\"");
    }

    #[test]
    fn expand_leaves_args_without_placeholder() {
        let args = vec!["redis-cli".to_string()];
        assert_eq!(expand(&args, "0"), vec!["redis-cli".to_string()]);
    }
}
