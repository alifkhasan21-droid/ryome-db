//! Wrapper Docker API (bollard): lifecycle container, status, logs.

use crate::{config::Config, service::ServiceDef, ui};
use anyhow::{anyhow, bail, Context, Result};
use bollard::{
    container::LogOutput,
    errors::Error as DockerError,
    models::{
        ContainerCreateBody, ContainerInspectResponse, EndpointSettings, HealthConfig,
        HealthStatusEnum, HostConfig, NetworkCreateRequest, NetworkingConfig, PortBinding, PortMap,
        RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, LogsOptionsBuilder,
        RemoveContainerOptionsBuilder, RemoveVolumeOptions, StopContainerOptionsBuilder,
    },
    Docker,
};
use futures_util::StreamExt;
use serde::Serialize;
use std::{
    collections::HashMap,
    io::Write,
    net::{SocketAddr, TcpListener},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

pub struct Engine {
    docker: Docker,
    pub cfg: Config,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub service: String,
    pub container: String,
    pub enabled: bool,
    pub state: String,
    pub health: String,
    pub port: Option<u16>,
    pub image: String,
}

#[derive(Debug, PartialEq)]
pub enum UpResult {
    Created,
    Started,
    AlreadyRunning,
}

#[derive(Debug, PartialEq)]
pub enum StopResult {
    Stopped,
    AlreadyStopped,
    NotCreated,
}

fn is_not_found(e: &DockerError) -> bool {
    matches!(
        e,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// Label tempat menyimpan fingerprint spec container (lihat `spec_fingerprint`).
const LBL_SPEC: &str = "dbx.spec";

/// Bacaan probe kesiapan. Cukup panjang untuk memberi waktu docker-proxy
/// menutup koneksi kalau backend menolak, cukup pendek supaya tidak terasa.
const PROBE_WAIT: Duration = Duration::from_millis(500);

/// Docker tidak punya kode error khusus untuk port bentrok — hanya pesan.
fn is_port_conflict(e: &DockerError) -> bool {
    let DockerError::DockerResponseServerError { message, .. } = e else {
        return false;
    };
    let m = message.to_lowercase();
    m.contains("port is already allocated") || m.contains("address already in use")
}

/// Host yang bisa di-connect dari luar container — dipakai untuk menampilkan
/// URL koneksi maupun untuk probe kesiapan.
///
/// `0.0.0.0`/`::` adalah alamat *bind*, bukan alamat tujuan → petakan ke
/// loopback.
pub fn connect_host(bind: &str) -> &str {
    match bind {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    }
}

/// Probe kesiapan service lewat port host (`0.0.0.0`/`::` → loopback).
///
/// Connect saja tidak cukup: docker-proxy sudah membuka port host begitu
/// container start, jadi connect akan "berhasil" walau aplikasi di dalamnya
/// belum siap. Yang membedakan:
///
/// - backend belum listen → proxy (atau iptables) menutup/menolak koneksi →
///   baca dapat EOF, atau connect-nya sendiri yang gagal → **belum siap**
/// - backend sudah listen → koneksi tetap terbuka → baca yang timeout → **siap**
///
/// Pendekatan ini tidak bergantung pada mode networking Docker (userland-proxy
/// maupun DNAT) dan tidak butuh akses routable ke IP container.
async fn port_ready(bind: &str, port: u16) -> bool {
    let host = connect_host(bind);
    // bind_address tervalidasi sebagai IP literal; IPv6 butuh bracket.
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let Ok(addr) = target.parse::<SocketAddr>() else {
        return false;
    };

    let mut stream =
        match tokio::time::timeout(PROBE_WAIT, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(s)) => s,
            _ => return false, // tidak ada yang listen
        };
    let mut buf = [0u8; 1];
    match tokio::time::timeout(PROBE_WAIT, stream.read(&mut buf)).await {
        Ok(Ok(0)) => false, // EOF: koneksi ditutup lagi = backend menolak
        _ => true,          // ada data, atau koneksi masih terbuka
    }
}

/// FNV-1a 64-bit.
///
/// Dipakai karena `DefaultHasher` tidak menjamin hasilnya stabil antar versi
/// Rust — hash yang berubah sendiri akan memicu drift-check palsu.
fn fnv1a(input: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // offset basis
    for b in input.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{h:016x}")
}

/// Fingerprint isi spec yang menentukan bentuk container: image, port, env,
/// cmd, healthcheck, plus bagian config global yang ikut mengubah HostConfig.
///
/// Disimpan sebagai label saat create, dibandingkan saat inspect → semua
/// perubahan config ketahuan, bukan cuma image dan port.
fn spec_fingerprint(s: &ServiceDef, cfg: &Config) -> String {
    let health = s.healthcheck.as_ref().map(|h| {
        (
            &h.cmd,
            h.interval_secs,
            h.timeout_secs,
            h.retries,
            h.start_period_secs,
        )
    });
    // Debug tuple: BTreeMap selalu terurut, jadi kanonik dan stabil.
    let spec = (
        s.image.as_str(),
        s.port,
        s.container_port,
        s.data_path.as_str(),
        &s.cmd,
        &s.env,
        health,
        cfg.bind_address.as_str(),
        cfg.network.trim(),
        cfg.restart_policy.as_str(),
    );
    fnv1a(&format!("{spec:?}"))
}

fn ns(secs: u64) -> i64 {
    (secs as i64).saturating_mul(1_000_000_000)
}

fn parse_restart(s: &str) -> Result<RestartPolicyNameEnum> {
    Ok(match s {
        "no" => RestartPolicyNameEnum::NO,
        "always" => RestartPolicyNameEnum::ALWAYS,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        other => bail!("restart_policy '{other}' tidak valid"),
    })
}

/// "repo/name:tag" -> ("repo/name", "tag")
fn split_image(image: &str) -> (&str, &str) {
    let after_slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    match image[after_slash..].rfind(':') {
        Some(i) => (&image[..after_slash + i], &image[after_slash + i + 1..]),
        None => (image, "latest"),
    }
}

pub fn port_free(bind: &str, port: u16) -> bool {
    TcpListener::bind((bind, port)).is_ok()
}

fn pick_port(bind: &str, want: u16, auto: bool) -> Result<u16> {
    if port_free(bind, want) {
        return Ok(want);
    }
    if !auto {
        bail!("port {want} sudah dipakai. Ganti `port` di file service, atau set auto_port = true");
    }
    let l = TcpListener::bind((bind, 0)).context("gagal mencari port kosong")?;
    Ok(l.local_addr()?.port())
}

fn host_port_from(map: Option<&PortMap>, cport: u16) -> Option<u16> {
    let binds = map?.get(&format!("{cport}/tcp"))?.as_ref()?;
    binds
        .iter()
        .find_map(|b| b.host_port.as_deref().and_then(|p| p.parse::<u16>().ok()))
}

fn host_port(info: &ContainerInspectResponse, cport: u16) -> Option<u16> {
    host_port_from(
        info.network_settings
            .as_ref()
            .and_then(|n| n.ports.as_ref()),
        cport,
    )
    .or_else(|| {
        host_port_from(
            info.host_config
                .as_ref()
                .and_then(|h| h.port_bindings.as_ref()),
            cport,
        )
    })
}

impl Engine {
    pub fn connect(cfg: Config) -> Result<Self> {
        let docker =
            Docker::connect_with_defaults().context("gagal inisialisasi koneksi Docker")?;
        Ok(Self { docker, cfg })
    }

    pub async fn check_daemon(&self) -> Result<()> {
        self.docker.ping().await.map(|_| ()).map_err(|e| {
            anyhow!(
                "Docker daemon tidak bisa diakses ({e})\n  \
                 → jalankan: sudo systemctl start docker\n  \
                 → pastikan user ada di group docker: sudo usermod -aG docker $USER (lalu login ulang)"
            )
        })
    }

    pub fn container_name(&self, s: &ServiceDef) -> String {
        format!("{}-{}", self.cfg.container_prefix, s.name)
    }

    pub fn volume_name(&self, s: &ServiceDef) -> String {
        format!("{}-{}-data", self.cfg.container_prefix, s.name)
    }

    async fn inspect(&self, name: &str) -> Result<Option<ContainerInspectResponse>> {
        match self.docker.inspect_container(name, None).await {
            Ok(i) => Ok(Some(i)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn status(&self, s: &ServiceDef) -> Result<Status> {
        let container = self.container_name(s);
        let mut st = Status {
            service: s.name.clone(),
            container: container.clone(),
            enabled: s.enabled,
            state: "absent".into(),
            health: "-".into(),
            port: None,
            image: s.image.clone(),
        };
        let Some(info) = self.inspect(&container).await? else {
            return Ok(st);
        };
        if let Some(state) = info.state.as_ref() {
            st.state = state
                .status
                .as_ref()
                .map(|x| x.to_string())
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| "unknown".into());
            if let Some(h) = state.health.as_ref().and_then(|h| h.status.as_ref()) {
                let h = h.to_string();
                if !h.is_empty() && h != "none" {
                    st.health = h;
                }
            }
        }
        st.port = host_port(&info, s.container_port);
        if let Some(img) = info.config.as_ref().and_then(|c| c.image.clone()) {
            st.image = img;
        }
        Ok(st)
    }

    /// Container harus sudah jalan. Return nama container.
    pub async fn require_running(&self, s: &ServiceDef) -> Result<String> {
        let name = self.container_name(s);
        let running = self
            .inspect(&name)
            .await?
            .and_then(|i| i.state)
            .and_then(|st| st.running)
            .unwrap_or(false);
        if !running {
            bail!("{} belum jalan — nyalakan dulu: dbx up {}", s.name, s.name);
        }
        Ok(name)
    }

    async fn ensure_network(&self) -> Result<()> {
        let net = self.cfg.network.trim();
        if net.is_empty() {
            return Ok(());
        }
        match self.docker.inspect_network(net, None).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => {
                match self
                    .docker
                    .create_network(NetworkCreateRequest {
                        name: net.to_string(),
                        driver: Some("bridge".into()),
                        labels: Some(HashMap::from([(
                            "dbx.managed".to_string(),
                            "true".to_string(),
                        )])),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(_) => Ok(()),
                    // `dbx up` start service paralel: bisa jadi service lain
                    // baru saja membuat network yang sama → bukan kegagalan.
                    Err(_) if self.docker.inspect_network(net, None).await.is_ok() => Ok(()),
                    Err(e) => Err(anyhow!(e).context(format!("gagal membuat network '{net}'"))),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        if !self.cfg.pull_missing {
            bail!("image {image} belum ada di lokal dan pull_missing = false");
        }
        ui::info(format!("pull {image} (bisa agak lama)…"));
        let t = Instant::now();
        let (repo, tag) = split_image(image);
        let opts = CreateImageOptionsBuilder::new()
            .from_image(repo)
            .tag(tag)
            .build();
        let mut stream = Box::pin(self.docker.create_image(Some(opts), None, None));
        while let Some(item) = stream.next().await {
            let info = item.with_context(|| format!("gagal pull {image}"))?;
            if let Some(msg) = info.error_detail.and_then(|d| d.message) {
                bail!("gagal pull {image}: {msg}");
            }
        }
        ui::info(format!("pull selesai ({:.1}s)", t.elapsed().as_secs_f32()));
        Ok(())
    }

    fn labels(&self, s: &ServiceDef) -> HashMap<String, String> {
        HashMap::from([
            ("dbx.managed".to_string(), "true".to_string()),
            ("dbx.service".to_string(), s.name.clone()),
        ])
    }

    fn create_body(
        &self,
        s: &ServiceDef,
        host_port: u16,
        volume: &str,
    ) -> Result<ContainerCreateBody> {
        let cport = format!("{}/tcp", s.container_port);
        let mut pm: PortMap = HashMap::new();
        pm.insert(
            cport.clone(),
            Some(vec![PortBinding {
                host_ip: Some(self.cfg.bind_address.clone()),
                host_port: Some(host_port.to_string()),
            }]),
        );

        let net = self.cfg.network.trim();
        let host_config = HostConfig {
            binds: Some(vec![format!("{volume}:{}", s.data_path)]),
            port_bindings: Some(pm),
            restart_policy: Some(RestartPolicy {
                name: Some(parse_restart(&self.cfg.restart_policy)?),
                maximum_retry_count: None,
            }),
            network_mode: (!net.is_empty()).then(|| net.to_string()),
            ..Default::default()
        };

        let networking_config = (!net.is_empty()).then(|| NetworkingConfig {
            endpoints_config: Some(HashMap::from([(
                net.to_string(),
                EndpointSettings {
                    aliases: Some(vec![s.name.clone()]),
                    ..Default::default()
                },
            )])),
        });

        let mut labels = self.labels(s);
        labels.insert(LBL_SPEC.to_string(), spec_fingerprint(s, &self.cfg));

        let healthcheck = s.healthcheck.as_ref().map(|h| {
            let mut test = vec!["CMD".to_string()];
            test.extend(h.cmd.iter().cloned());
            HealthConfig {
                test: Some(test),
                interval: Some(ns(h.interval_secs)),
                timeout: Some(ns(h.timeout_secs)),
                retries: Some(h.retries as i64),
                start_period: Some(ns(h.start_period_secs)),
                start_interval: None,
            }
        });

        Ok(ContainerCreateBody {
            image: Some(s.image.clone()),
            env: Some(s.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            cmd: (!s.cmd.is_empty()).then(|| s.cmd.clone()),
            exposed_ports: Some(vec![cport]),
            healthcheck,
            labels: Some(labels),
            host_config: Some(host_config),
            networking_config,
            ..Default::default()
        })
    }

    /// Bandingkan spec di config dengan spec yang tercatat di label container.
    ///
    /// Container buatan dbx versi lama (tanpa label `dbx.spec`) jatuh ke cek
    /// image saja, supaya tidak memunculkan peringatan palsu terus-menerus.
    fn drift(&self, info: &ContainerInspectResponse, s: &ServiceDef) -> Option<String> {
        let labels = info.config.as_ref()?.labels.as_ref();
        if let Some(want) = labels.and_then(|l| l.get(LBL_SPEC)) {
            if *want != spec_fingerprint(s, &self.cfg) {
                let old = info.config.as_ref().and_then(|c| c.image.as_deref());
                let what = match old {
                    Some(img) if img != s.image => format!("image {img} → {}", s.image),
                    _ => "image / port / env / healthcheck / cmd / bind / network".into(),
                };
                return Some(format!(
                    "config berubah ({what}) → dbx up {} --recreate",
                    s.name
                ));
            }
            return None;
        }
        let img = info.config.as_ref()?.image.as_deref()?;
        if img != s.image {
            return Some(format!(
                "container masih pakai image {img}, config minta {} → dbx up {} --recreate",
                s.image, s.name
            ));
        }
        None
    }

    /// Buat + start container (tanpa menunggu sehat). Data di volume tetap aman saat recreate.
    pub async fn up(&self, s: &ServiceDef, recreate: bool) -> Result<UpResult> {
        let name = self.container_name(s);
        let mut existing = self.inspect(&name).await?;

        if recreate && existing.is_some() {
            self.remove(s, false).await?;
            existing = None;
        }

        if let Some(info) = existing {
            if let Some(msg) = self.drift(&info, s) {
                ui::warn(format!("{}: {msg}", s.name));
            }
            let running = info.state.as_ref().and_then(|x| x.running).unwrap_or(false);
            if running {
                return Ok(UpResult::AlreadyRunning);
            }
            self.docker
                .start_container(&name, None)
                .await
                .with_context(|| format!("gagal start {name}"))?;
            return Ok(UpResult::Started);
        }

        self.ensure_image(&s.image).await?;
        self.ensure_network().await?;

        let volume = self.volume_name(s);
        self.docker
            .create_volume(VolumeCreateRequest {
                name: Some(volume.clone()),
                labels: Some(self.labels(s)),
                ..Default::default()
            })
            .await
            .with_context(|| format!("gagal membuat volume {volume}"))?;

        // `pick_port` melepas listener-nya sebelum Docker-proxy sempat bind,
        // jadi ada celah TOCTOU: port bisa diambil proses lain di antaranya.
        // Kalau kena, ulang dengan port baru — volume data tidak tersentuh.
        let max_try = if self.cfg.auto_port { 3 } else { 1 };
        let mut conflict: Option<DockerError> = None;

        for attempt in 0..max_try {
            let port = pick_port(&self.cfg.bind_address, s.port, self.cfg.auto_port)?;
            if port != s.port {
                ui::warn(format!(
                    "{}: port {} dipakai proses lain, pakai {port}",
                    s.name, s.port
                ));
            }

            let body = self.create_body(s, port, &volume)?;
            let opts = CreateContainerOptionsBuilder::new().name(&name).build();
            self.docker
                .create_container(Some(opts), body)
                .await
                .with_context(|| format!("gagal membuat container {name}"))?;

            let e = match self.docker.start_container(&name, None).await {
                Ok(()) => return Ok(UpResult::Created),
                Err(e) => e,
            };
            if !is_port_conflict(&e) {
                return Err(anyhow!(e).context(format!(
                    "gagal start {name} (cek port / `docker logs {name}`)"
                )));
            }

            conflict = Some(e);
            let last = attempt + 1 == max_try;
            if !last {
                ui::warn(format!(
                    "{}: port {port} keburu diambil proses lain, coba port lain",
                    s.name
                ));
            }
            // Buang container yang gagal start: binding port-nya sudah
            // terlanjur tercatat, jadi percobaan berikutnya (atau `dbx up`
            // ulang oleh user) harus membuat ulang. Volume data tak tersentuh.
            self.remove(s, false).await?;
            if last {
                break;
            }
        }

        // Loop di atas hanya keluar lewat `break`, yang sudah diawali
        // mengisi `conflict`. Cabang `None` pengaman agar tidak ada panic.
        Err(match conflict {
            Some(e) => anyhow!(e).context(format!(
                "gagal start {name}: port bentrok dengan proses lain \
                 (sudah dicoba {max_try}x) — matikan proses pemakai port itu, \
                 atau set auto_port = true"
            )),
            None => anyhow!("gagal start {name}"),
        })
    }

    /// Tunggu sampai service siap menerima koneksi:
    /// - ada `[healthcheck]` → sampai status `HEALTHY`
    /// - tidak ada → sampai port host benar-benar melayani (lihat `port_ready`)
    ///
    /// Tanpa fallback kedua, service tanpa healthcheck dianggap siap begitu
    /// container running — padahal proses di dalamnya (initdb, dll) mungkin
    /// belum selesai.
    pub async fn wait_ready(&self, s: &ServiceDef) -> Result<()> {
        let name = self.container_name(s);
        let limit = Duration::from_secs(self.cfg.wait_timeout_secs);
        let start = Instant::now();
        let has_hc = s.healthcheck.is_some();

        loop {
            let info = self
                .inspect(&name)
                .await?
                .ok_or_else(|| anyhow!("container {name} hilang"))?;
            let state = info.state.as_ref();
            let running = state.and_then(|x| x.running).unwrap_or(false);
            let restarting = state.and_then(|x| x.restarting).unwrap_or(false);

            if !running && !restarting {
                let code = state.and_then(|x| x.exit_code).unwrap_or(-1);
                bail!(
                    "{} berhenti (exit code {code}) — cek: dbx logs {}",
                    s.name,
                    s.name
                );
            }

            if has_hc {
                let healthy = state
                    .and_then(|x| x.health.as_ref())
                    .and_then(|h| h.status.as_ref())
                    .is_some_and(|h| *h == HealthStatusEnum::HEALTHY);
                if healthy {
                    return Ok(());
                }
            } else if let Some(port) = host_port(&info, s.container_port) {
                if port_ready(&self.cfg.bind_address, port).await {
                    return Ok(());
                }
                // port belum terbuka → terus tunggu sampai limit
            }

            if start.elapsed() > limit {
                if has_hc {
                    bail!(
                        "timeout {}s menunggu {} sehat — cek: dbx logs {}",
                        self.cfg.wait_timeout_secs,
                        s.name,
                        s.name
                    );
                }
                bail!(
                    "timeout {}s menunggu {} melayani koneksi di port {} \
                     (service ini tidak punya [healthcheck]) — cek: dbx logs {}",
                    self.cfg.wait_timeout_secs,
                    s.name,
                    s.port,
                    s.name
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn stop(&self, s: &ServiceDef) -> Result<StopResult> {
        let name = self.container_name(s);
        match self.inspect(&name).await? {
            None => Ok(StopResult::NotCreated),
            Some(info) if !info.state.as_ref().and_then(|x| x.running).unwrap_or(false) => {
                Ok(StopResult::AlreadyStopped)
            }
            Some(_) => {
                let opts = StopContainerOptionsBuilder::new()
                    .t(self.cfg.stop_timeout_secs)
                    .build();
                self.docker
                    .stop_container(&name, Some(opts))
                    .await
                    .with_context(|| format!("gagal stop {name}"))?;
                Ok(StopResult::Stopped)
            }
        }
    }

    /// Hapus container. Kalau `volumes` true, volume data ikut dihapus.
    ///
    /// Container yang masih jalan di-stop dulu secara graceful (hormati
    /// `stop_timeout_secs`) sebelum dihapus. Tanpa ini Docker akan SIGKILL
    /// langsung lewat `force`, dan DB yang sedang menulis bisa kotor.
    pub async fn remove(&self, s: &ServiceDef, volumes: bool) -> Result<()> {
        let name = self.container_name(s);
        self.stop(s).await?;
        let opts = RemoveContainerOptionsBuilder::new().force(true).build();
        match self.docker.remove_container(&name, Some(opts)).await {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(anyhow!(e).context(format!("gagal menghapus {name}"))),
        }
        if volumes {
            let vol = self.volume_name(s);
            match self
                .docker
                .remove_volume(&vol, None::<RemoveVolumeOptions>)
                .await
            {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(anyhow!(e).context(format!("gagal menghapus volume {vol}"))),
            }
        }
        Ok(())
    }

    pub async fn logs(&self, s: &ServiceDef, follow: bool, tail: &str) -> Result<()> {
        let name = self.container_name(s);
        if self.inspect(&name).await?.is_none() {
            bail!("container {name} belum ada — jalankan: dbx up {}", s.name);
        }
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .follow(follow)
            .tail(tail)
            .build();
        let mut stream = Box::pin(self.docker.logs(&name, Some(opts)));
        let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
        while let Some(chunk) = stream.next().await {
            let res = match chunk? {
                LogOutput::StdErr { message } => err.write_all(&message),
                other => out.write_all(other.as_ref()),
            };
            if res.is_err() {
                break; // pipe ditutup (misal | head)
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::service::{Client, Healthcheck, ServiceDef};
    use std::path::PathBuf;

    #[test]
    fn split() {
        assert_eq!(split_image("postgres:17"), ("postgres", "17"));
        assert_eq!(split_image("redis"), ("redis", "latest"));
        assert_eq!(
            split_image("docker.elastic.co/elasticsearch/elasticsearch:8.17.0"),
            ("docker.elastic.co/elasticsearch/elasticsearch", "8.17.0")
        );
        assert_eq!(
            split_image("localhost:5000/foo"),
            ("localhost:5000/foo", "latest")
        );
    }

    fn svc() -> ServiceDef {
        ServiceDef {
            name: "postgres".into(),
            aliases: vec![],
            enabled: true,
            image: "postgres:17".into(),
            port: 5432,
            container_port: 5432,
            data_path: "/var/lib/postgresql/data".into(),
            cmd: vec![],
            env: [("POSTGRES_PASSWORD".to_string(), "postgres".to_string())]
                .into_iter()
                .collect(),
            healthcheck: Some(Healthcheck {
                cmd: vec!["pg_isready".into(), "-U".into(), "postgres".into()],
                interval_secs: 3,
                timeout_secs: 3,
                retries: 20,
                start_period_secs: 0,
            }),
            client: Client::default(),
            file: PathBuf::new(),
        }
    }

    #[test]
    fn spec_fingerprint_is_stable_and_detects_changes() {
        let cfg = Config::default();
        let base = spec_fingerprint(&svc(), &cfg);

        // spec identik → hash identik (harusnya tidak ada drift palsu)
        assert_eq!(base, spec_fingerprint(&svc(), &cfg));

        // env berubah → drift
        let mut s = svc();
        s.env.insert("POSTGRES_PASSWORD".into(), "rahasia".into());
        assert_ne!(base, spec_fingerprint(&s, &cfg));

        // image berubah → drift
        let mut s = svc();
        s.image = "postgres:18".into();
        assert_ne!(base, spec_fingerprint(&s, &cfg));

        // bind_address berubah (ikut HostConfig) → drift
        let cfg2 = Config {
            bind_address: "0.0.0.0".into(),
            ..Config::default()
        };
        assert_ne!(base, spec_fingerprint(&svc(), &cfg2));

        // network dikosongkan → drift
        let cfg3 = Config {
            network: String::new(),
            ..Config::default()
        };
        assert_ne!(base, spec_fingerprint(&svc(), &cfg3));
    }

    #[test]
    fn spec_fingerprint_ignores_unrelated_config() {
        let cfg = Config::default();
        let base = spec_fingerprint(&svc(), &cfg);
        // default_services & backup_dir tidak mempengaruhi bentuk container
        let cfg2 = Config {
            default_services: vec!["redis".into()],
            backup_dir: "/tmp/b".into(),
            ..Config::default()
        };
        assert_eq!(base, spec_fingerprint(&svc(), &cfg2));
    }

    #[test]
    fn pick_port_falls_back_when_busy() {
        use std::net::TcpListener;
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = l.local_addr().unwrap().port();

        // auto_port = true → dapat port lain yang bebas
        let alt = pick_port("127.0.0.1", busy, true).unwrap();
        assert_ne!(alt, busy);
        assert!(port_free("127.0.0.1", alt));

        // auto_port = false → menolak dengan pesan jelas
        let err = format!("{:#}", pick_port("127.0.0.1", busy, false).unwrap_err());
        assert!(err.contains("sudah dipakai"), "pesan: {err}");
    }

    #[test]
    fn detects_port_conflict_error() {
        let conflict = DockerError::DockerResponseServerError {
            status_code: 500,
            message: "driver failed programming external connectivity on endpoint dbx-postgres: \
                      Bind for 127.0.0.1:5432 failed: port is already allocated"
                .into(),
        };
        assert!(is_port_conflict(&conflict));

        let conflict = DockerError::DockerResponseServerError {
            status_code: 500,
            message: "listen tcp4 127.0.0.1:6379: bind: address already in use".into(),
        };
        assert!(is_port_conflict(&conflict));

        // jangan tertukar dengan error lain
        let other = DockerError::DockerResponseServerError {
            status_code: 404,
            message: "No such container: dbx-postgres".into(),
        };
        assert!(!is_port_conflict(&other));
    }

    #[tokio::test]
    async fn port_ready_distinguishes_backend_states() {
        use tokio::io::AsyncWriteExt;

        // 1. tidak ada yang listen → belum siap
        let closed = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        }; // listener dilepas → port kosong
        assert!(!port_ready("127.0.0.1", closed).await);

        // 2. proxy accept lalu menutup (backend menolak) → EOF → belum siap
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let p = l.local_addr().unwrap().port();
        let t = tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let _ = s.shutdown().await; // EOF persis seperti docker-proxy
            }
        });
        assert!(
            !port_ready("127.0.0.1", p).await,
            "EOF harus dianggap belum siap"
        );
        let _ = t.await;

        // 3. proxy accept, backend merespons → ada data → siap
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let p = l.local_addr().unwrap().port();
        let t = tokio::spawn(async move {
            if let Ok((mut s, _)) = l.accept().await {
                let _ = s.write_all(b"\0").await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        assert!(port_ready("127.0.0.1", p).await, "data masuk = siap");
        let _ = t.await;
    }

    #[test]
    fn connect_host_maps_wildcard_bind_to_loopback() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("::"), "127.0.0.1");
        assert_eq!(connect_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(connect_host("192.168.1.9"), "192.168.1.9");
        assert_eq!(connect_host("::1"), "::1", "loopback IPv6 dipertahankan");
    }

    #[test]
    fn probe_target_parses_for_v4_and_v6() {
        // memastikan format host:port valid untuk kedua keluarga IP
        for (bind, port, want) in [
            ("127.0.0.1", 5432u16, "127.0.0.1:5432"),
            ("0.0.0.0", 5432, "127.0.0.1:5432"),
            ("::1", 5432, "[::1]:5432"),
            ("::", 6379, "127.0.0.1:6379"),
        ] {
            let host = connect_host(bind);
            let target = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            assert_eq!(target, want, "bind {bind}");
            assert!(
                target.parse::<SocketAddr>().is_ok(),
                "{target} harus valid SocketAddr"
            );
        }
    }
}
