# dbx

[![CI](https://github.com/alifkhasan21-droid/ryome-db/actions/workflows/ci.yml/badge.svg)](https://github.com/alifkhasan21-droid/ryome-db/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)

> CLI Rust untuk menjalankan database di Docker — **tanpa satu pun terinstall ke system**.
> Definisi ada di file config, bukan hardcode: nambah database baru = nambah 1 file `.toml`.

PostgreSQL · Redis · MariaDB · MongoDB · Elasticsearch — atau engine apa pun yang punya image Docker.

## Install

**Satu baris:**

```bash
curl -fsSL https://raw.githubusercontent.com/alifkhasan21-droid/ryome-db/main/install.sh | sh
```

Script itu akan:

1. mendeteksi OS & arsitektur otomatis (Linux/macOS · x86_64/aarch64)
2. mengunduh binary dari GitHub Release terbaru → `~/.local/bin/dbx`
3. kalau rilis untuk platform itu belum ada → membangun dari source (butuh `cargo`)

Tujuan bisa diganti lewat env, misal ke `/usr/local/bin`:

```bash
sudo DBX_INSTALL_DIR=/usr/local/bin sh -c \
  'curl -fsSL https://raw.githubusercontent.com/alifkhasan21-droid/ryome-db/main/install.sh | sh'
```

Alternatif lain, lewat cargo:

```bash
cargo install --git https://github.com/alifkhasan21-droid/ryome-db.git
```

Verifikasi sekaligus cek lingkungan:

```bash
dbx --version && dbx doctor
```

### Syarat

- **Docker daemon** jalan + CLI `docker` (dipakai untuk `sh`, `dump`, `restore`)
  - Arch: `sudo pacman -S docker && sudo systemctl enable --now docker.socket`
    lalu `sudo usermod -aG docker $USER` (**login ulang** supaya group aktif)
- **Podman**: `export DOCKER_HOST=unix:///run/user/$UID/podman/podman.sock`

## Mulai cepat

```bash
dbx doctor                  # cek docker, config, default_services, port
dbx up                      # nyalakan default_services (postgres, redis)
dbx url pg --db myapp       # → postgresql://postgres:postgres@127.0.0.1:5432/myapp
dbx createdb pg myapp
dbx sh pg                   # psql, langsung di dalam container
dbx ls                      # status semua service
dbx down                    # stop semua
```

Di project kamu cukup:

```bash
export DATABASE_URL=$(dbx url pg --db myapp)
```

## Perintah

| perintah | fungsi |
|---|---|
| `dbx doctor` | cek docker, config, `default_services`, port |
| `dbx up [svc...]` | nyalakan — tanpa argumen = `default_services` |
| `dbx down [svc...]` | stop — tanpa argumen = semua |
| `dbx restart [svc...]` | restart |
| `dbx rm <svc> [--volumes]` | hapus container · `--volumes` hapus juga data (ada konfirmasi) |
| `dbx ls [--json] [--all]` | status service · `--all` termasuk yang `enabled = false` |
| `dbx logs <svc> [-f]` | log container |
| `dbx sh <svc>` | client interaktif (`psql`, `redis-cli`, …) |
| `dbx url <svc> [--db NAME]` | connection string |
| `dbx createdb <svc> <db>` | buat database baru |
| `dbx dump <svc> <db> [-o FILE]` | backup · default ke `backup_dir` · `-o -` ke stdout |
| `dbx restore <svc> <db> [FILE]` | restore dari file, atau `cat x.sql \| dbx restore …` |
| `dbx init` | buat config default |
| `dbx config [edit [svc] \| show]` | lihat / edit config |
| `dbx completions <shell>` | completion untuk bash, zsh, fish, … |

Nama service boleh memakai alias — `dbx up pg` sama dengan `dbx up postgres`.

Completion contohnya:

```bash
dbx completions fish > ~/.config/fish/completions/dbx.fish
dbx completions bash  > /etc/bash_completion.d/dbx    # atau ~/.local/share/bash-completion/completions/dbx
```

## Config

Semua pengaturan ada di `~/.config/dbx/` — dibuat otomatis saat pertama jalan, atau manual lewat `dbx init`:

```
~/.config/dbx/
├── config.toml            # pengaturan global
└── services/
    ├── postgres.toml      # 1 file = 1 database
    ├── mariadb.toml
    ├── redis.toml
    ├── mongodb.toml       # enabled = false (nyalakan kalau perlu)
    └── elasticsearch.toml # enabled = false
```

```bash
dbx config                 # lokasi folder + daftar service
dbx config edit            # buka config.toml di $EDITOR
dbx config edit pg         # buka services/postgres.toml
dbx config show
```

Override lokasi: `--config-dir DIR` atau env `DBX_CONFIG_DIR`.
**Typo di key config langsung ditolak** dengan pesan yang menunjukkan field yang benar — bukan diam-diam diabaikan.

### `config.toml`

| key | default | keterangan |
|---|---|---|
| `container_prefix` | `dbx` | prefix nama container/volume → `dbx-postgres`, `dbx-postgres-data` |
| `network` | `dbx` | network bersama; container lain bisa akses via nama service. `""` = nonaktif |
| `bind_address` | `127.0.0.1` | interface publish port. Jangan `0.0.0.0` kecuali perlu (Docker bypass firewall) |
| `restart_policy` | `unless-stopped` | `no` / `always` / `unless-stopped` / `on-failure` |
| `pull_missing` | `true` | auto pull image |
| `auto_port` | `true` | port bentrok → pilih port kosong otomatis (maks. 3 percobaan) |
| `wait_timeout_secs` | `90` | batas tunggu service siap |
| `stop_timeout_secs` | `10` | batas tunggu graceful stop sebelum di-kill |
| `default_services` | `["postgres","redis"]` | yang dinyalakan `dbx up` tanpa argumen |
| `backup_dir` | `""` | kosong = `~/.local/share/dbx/backups`; boleh `~/...` |

### `services/<nama>.toml`

```toml
name = "postgres"
aliases = ["pg"]
enabled = true
image = "postgres:17"          # pin versi major
port = 5432                    # port host
container_port = 5432
data_path = "/var/lib/postgresql/data"
# cmd = ["redis-server", "--appendonly", "yes"]   # override command (opsional)

[env]
POSTGRES_PASSWORD = "postgres"

[healthcheck]
cmd = ["pg_isready", "-U", "postgres"]
interval_secs = 3
timeout_secs = 3
retries = 20
start_period_secs = 0

[client]                       # dipakai sh / url / createdb / dump / restore
default_db = "postgres"
url = "postgresql://postgres:postgres@{host}:{port}/{db}"
shell = ["psql", "-U", "postgres"]
create_db = ["psql", "-U", "postgres", "-c", "CREATE DATABASE \"{db}\""]
dump = ["pg_dump", "-U", "postgres", "{db}"]        # tulis ke stdout
restore = ["psql", "-U", "postgres", "-d", "{db}"]  # baca dari stdin
dump_ext = "sql"
```

Placeholder yang tersedia: `{host}`, `{port}`, `{db}`.
Field `client.*` boleh dikosongkan kalau fiturnya tidak relevan (mis. Redis tidak punya `dump`).

`[healthcheck]` bersifat **opsional**. Kalau tidak diisi, `dbx up` **bukan** langsung
dianggap selesai: dbx menunggu port service benar-benar melayani koneksi (batas
`wait_timeout_secs`), karena container `running` belum tentu artinya proses di
dalamnya sudah siap.

### Menambah database baru

1. Salin template yang sudah ada:
   ```bash
   cp ~/.config/dbx/services/postgres.toml ~/.config/dbx/services/mydb.toml
   ```
2. Sesuaikan `name`, `aliases`, `image`, `port`, `[env]`, dan `[client]`
3. Nyalakan:
   ```bash
   dbx up mydb
   ```

Bisa juga tulis dari nol — field wajib hanya `name`, `image`, `port`, dan `container_port`.

> Mengubah `image`/`port`/`env` setelah container sudah ada? Jalankan
> `dbx up <svc> --recreate` — container dibuat ulang, **data di volume aman**.

## Keamanan & perilaku

- **Password default hanya untuk dev lokal.** Port di-bind ke `127.0.0.1`, jadi
  tidak terekspos ke jaringan lokal.
- **Stop itu graceful.** `dbx down`, `dbx rm`, dan `dbx up --recreate` menghormati
  `stop_timeout_secs` sebelum memaksa — DB tidak dibunuh paksa (SIGKILL) di tengah write.
- **Data di volume terpisah dari container.** Menghapus container tidak menghapus
  data, kecuali `dbx rm --volumes` (ada konfirmasi) atau `docker volume rm`.
- **Port tidak bentrok diam-diam.** Kalau `auto_port = true` dan port terpakai,
  dbx memilih port kosong; kalau kalah race dengan proses lain, dia mencoba lagi
  dengan port baru.
- Lifecycle container memakai Docker API (crate `bollard`); `sh`/`dump`/`restore`
  memanggil `docker exec` supaya TTY & pipe stdin/stdout asli.
- `dbx ls` tetap menampilkan daftar service walau Docker daemon tidak terjangkau
  (kolom STATE jadi `unknown`), supaya daftar config tetap bisa dilihat.

## Pengembangan

Perintah yang dijalankan CI (`.github/workflows/ci.yml`) di tiap push/PR —
jalankan juga sebelum commit:

```bash
cargo fmt                                  # lalu: cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                 # unit test, tidak butuh Docker
```

Push tag `v*` (mis. `v0.1.0`) akan memicu build release multi-platform otomatis.

## Struktur kode

```
src/main.rs       entry point (+ penanganan SIGPIPE untuk pipe ke | head)
src/cli.rs        definisi command (clap)
src/commands.rs   implementasi tiap command
src/config.rs     config.toml + lokasi folder + init
src/service.rs    parsing services/*.toml
src/docker.rs     bollard: up/stop/rm/status/logs/wait siap + drift check
src/exec.rs       docker exec (sh/dump/restore)
src/ui.rs         warna & format output
defaults/         template config yang di-embed ke binary
install.sh        installer satu-baris (dipakai README)
```

## Lisensi

MIT — lihat [LICENSE](LICENSE).
