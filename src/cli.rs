use clap::{Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dbx",
    version,
    about = "Kelola database di Docker tanpa install ke system",
    after_help = "Config: ~/.config/dbx (config.toml + services/*.toml). Lihat `dbx config`."
)]
pub struct Cli {
    /// Folder config (default: ~/.config/dbx)
    #[arg(long, global = true, env = "DBX_CONFIG_DIR", value_name = "DIR")]
    pub config_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Buat folder config + file default
    Init {
        /// Timpa file yang sudah ada
        #[arg(long)]
        force: bool,
    },

    /// Nyalakan database (tanpa argumen = default_services)
    Up {
        services: Vec<String>,
        /// Jangan tunggu sampai healthy
        #[arg(long)]
        no_wait: bool,
        /// Buat ulang container (data di volume tetap aman)
        #[arg(long)]
        recreate: bool,
    },

    /// Stop database (tanpa argumen = semua)
    Down { services: Vec<String> },

    /// Restart database (tanpa argumen = default_services)
    Restart { services: Vec<String> },

    /// Hapus container (opsional beserta volume data)
    Rm {
        #[arg(required = true)]
        services: Vec<String>,
        /// Hapus juga volume data (PERMANEN)
        #[arg(long)]
        volumes: bool,
        /// Lewati konfirmasi
        #[arg(short, long)]
        yes: bool,
    },

    /// Daftar service + status
    Ls {
        /// Termasuk yang enabled = false
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Lihat log container
    Logs {
        service: String,
        #[arg(short, long)]
        follow: bool,
        /// Jumlah baris terakhir ("all" untuk semua)
        #[arg(short = 'n', long, default_value = "100")]
        tail: String,
    },

    /// Buka client interaktif (psql, redis-cli, ...)
    Sh {
        service: String,
        /// Argumen tambahan diteruskan apa adanya ke client
        /// (contoh: dbx sh pg -c "select 1")
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Print connection string
    Url {
        service: String,
        #[arg(long)]
        db: Option<String>,
    },

    /// Buat database baru di dalam service
    Createdb { service: String, db: String },

    /// Dump database ke file (default: folder backup_dir)
    Dump {
        service: String,
        db: String,
        /// File tujuan, atau "-" untuk stdout
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Restore dump ke database (dari file, atau stdin)
    Restore {
        service: String,
        db: String,
        file: Option<PathBuf>,
    },

    /// Lihat / edit config
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },

    /// Cek kesiapan environment (docker, config, port)
    Doctor,

    /// Generate shell completion
    Completions { shell: Shell },
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print lokasi folder config
    Path,
    /// Print isi config.toml
    Show,
    /// Buka config.toml (atau file service) di $EDITOR
    Edit {
        /// Nama service; kosong = config.toml
        service: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Argumen setelah nama service harus diteruskan apa adanya, termasuk
    /// yang diawali `-` (klaim di help: "dbx sh pg -c \"select 1\"").
    #[test]
    fn sh_meneruskan_argumen_tambahan() {
        let cli = Cli::try_parse_from(["dbx", "sh", "pg", "-c", "select 1"]).unwrap();
        let Cmd::Sh { service, args } = cli.cmd else {
            panic!("bukan perintah sh");
        };
        assert_eq!(service, "pg");
        assert_eq!(args, ["-c", "select 1"]);
    }

    #[test]
    fn sh_tanpa_argumen_tambahan_tetap_interaktif() {
        let cli = Cli::try_parse_from(["dbx", "sh", "pg"]).unwrap();
        let Cmd::Sh { service, args } = cli.cmd else {
            panic!("bukan perintah sh");
        };
        assert_eq!(service, "pg");
        assert!(
            args.is_empty(),
            "harus kosong supaya membuka shell interaktif"
        );
    }

    /// `--` biasa seharusnya tetap berfungsi sebagai penanda akhir option.
    #[test]
    fn sh_menerima_double_dash() {
        let cli = Cli::try_parse_from(["dbx", "sh", "pg", "--", "-c", "select 1"]).unwrap();
        let Cmd::Sh { service, args } = cli.cmd else {
            panic!("bukan perintah sh");
        };
        assert_eq!(service, "pg");
        assert_eq!(args, ["-c", "select 1"]);
    }
}
