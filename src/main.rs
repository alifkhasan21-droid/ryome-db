mod cli;
mod commands;
mod config;
mod docker;
mod exec;
mod service;
mod ui;

use clap::Parser;

#[tokio::main]
async fn main() {
    // Rust mengabaikan SIGPIPE sejak proses dimulai, sehingga menulis ke pipe
    // yang sudah ditutup (`dbx url pg | head`, `dbx completions | head`)
    // menghasilkan EPIPE yang berujung panic (exit 101). Kembalikan perilaku
    // Unix: mati diam-diam lewat sinyal, seperti `grep`/`ls`/`docker`.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = cli::Cli::parse();
    if let Err(e) = commands::run(cli).await {
        eprintln!("{} {:#}", ui::red("error:"), e);
        std::process::exit(1);
    }
}
