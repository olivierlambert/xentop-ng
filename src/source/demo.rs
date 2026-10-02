//! A simulated Xen host, so the UI can be developed and demonstrated
//! anywhere. Loads follow smooth periodic patterns with random bursts, disk
//! latency grows with queueing, steal time grows with pCPU contention, and
//! short-lived CI domains come and go.

use super::{Avail, DataStatus, Source, XapiState};
use crate::model::*;
use anyhow::Result;
use std::f64::consts::TAU;
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform in [0, 1).
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f()
    }
    fn uuid(&mut self) -> String {
        let (a, b) = (self.next(), self.next());
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xffff,
            b >> 48,
            b & 0xffff_ffff_ffff
        )
    }
}

/// Simulated SRs: (name, kind, latency multiplier, IOPS knee). Names are
/// only for the code; like on a real host without xapi, the UI shows UUIDs.
const SRS: [(&str, &str, f64, f64); 3] = [
    ("Local SSD", "ext", 0.5, 120_000.0),
    ("NFS VM store", "nfs", 2.2, 12_000.0),
    ("iSCSI array", "lvmoiscsi", 1.3, 40_000.0),
];
const SR_SSD: usize = 0;
const SR_NFS: usize = 1;
const SR_ISCSI: usize = 2;

/// Which SR a VM's disk `i` lives on.
fn sr_for(name: &str, i: usize) -> usize {
    let first = |sr: usize| if i == 0 { sr } else { SR_ISCSI };
    if name.starts_with("pg-") || name.starts_with("ml-") || name.starts_with("hana") {
        first(SR_SSD)
    } else if name.starts_with("IO-lab") {
        SR_SSD
    } else if name.starts_with("win") {
        SR_ISCSI
    } else {
        SR_NFS
    }
}

#[derive(Clone)]
struct Profile {
    /// Mean utilisation per vCPU (0..1).
    cpu: f64,
    /// Amplitude of the slow oscillation.
    swing: f64,
    period: f64,
    /// Probability per second of a burst starting.
    burst_p: f64,
    rd_iops: f64,
    wr_iops: f64,
    /// Average request size (bytes).
    io_size: f64,
    /// Unloaded service time (µs).
    lat_us: f64,
    /// IOPS at which latency doubles.
    iops_knee: f64,
    rx_bps: f64,
    tx_bps: f64,
}

struct SimDisk {
    kind: VbdKind,
    share: f64,
    raw: VbdRaw,
    ext: VbdExt,
    /// Index into `DemoSource::srs`.
    sr: usize,
}

/// A simulated storage repository. Each has its own latency profile, and
/// gets slower as the VMs on it push it towards its IOPS knee.
struct SimSr {
    uuid: String,
    name: &'static str,
    kind: &'static str,
    lat_mult: f64,
    /// Total IOPS at which the SR's latency has grown by 60%.
    knee: f64,
    /// IOPS over the last step, smoothed.
    iops: f64,
}

struct SimDom {
    id: u32,
    name: String,
    uuid: String,
    mem: u64,
    max_mem: u64,
    /// Balloon target, when different from `mem` (ballooning in progress).
    mem_target: Option<u64>,
    prof: Profile,
    phase: f64,
    burst: f64,
    vcpu_util: Vec<f64>,
    vcpus: Vec<VcpuRaw>,
    disks: Vec<SimDisk>,
    nets: Vec<NetRaw>,
    paused: bool,
    /// Seconds left to live (CI jobs).
    ttl: Option<f64>,
}

pub struct DemoSource {
    stock: bool,
    rng: Rng,
    /// Simulated time (s); decoupled from the wall clock so the host can
    /// be run forward during calibration.
    sim_t: f64,
    last: Instant,
    pcpus: u32,
    mem: u64,
    /// CI jobs arrive faster on bigger hosts.
    ci_scale: f64,
    /// Guest load multiplier from normalisation; also applied to bursts,
    /// jitter and CI jobs so the host stays near its target.
    load_k: f64,
    /// Guest memory multiplier, applied to CI jobs too.
    mem_k: f64,
    pcpu_idle: Vec<u64>,
    srs: Vec<SimSr>,
    doms: Vec<SimDom>,
    next_id: u32,
    ci_seq: u32,
    ci_timer: f64,
}

fn prof(cpu: f64, swing: f64, period: f64, burst_p: f64) -> Profile {
    Profile {
        cpu: cpu * 0.55,
        swing: swing * 0.6,
        period,
        burst_p,
        rd_iops: 0.0,
        wr_iops: 0.0,
        io_size: 16384.0,
        lat_us: 250.0,
        iops_knee: 20000.0,
        rx_bps: 0.0,
        tx_bps: 0.0,
    }
}

/// Shape of the simulated host.
#[derive(Clone, Debug)]
pub struct DemoConfig {
    pub pcpus: u32,
    pub mem: u64,
    /// Average host busy fraction the fleet is tuned for (0..1).
    pub cpu_load: f64,
    /// Fraction of RAM assigned to domains (0..1).
    pub mem_use: f64,
    /// Pretend to be a stock libxenstat without fallbacks.
    pub stock: bool,
}

impl Default for DemoConfig {
    fn default() -> Self {
        DemoConfig {
            pcpus: 16,
            mem: 128 * GIB,
            cpu_load: 0.55,
            mem_use: 0.80,
            stock: false,
        }
    }
}

/// Round a GiB amount to a size people actually give VMs (0.5, 1, 2, 3, 4,
/// 6, 8, 12, 16, 24, ...), in bytes.
fn nice_gib(g: f64) -> u64 {
    let mut best = 0.5f64;
    let mut v = 0.5f64;
    while v <= 65536.0 {
        for c in [v, v * 1.5] {
            if (c - g).abs() < (best - g).abs() {
                best = c;
            }
        }
        v *= 2.0;
    }
    (best * GIB as f64) as u64
}

impl DemoSource {
    /// A simulated host. The fleet grows with it: VM counts follow the pCPU
    /// count, VM sizes follow the RAM, and loads are normalised so the host
    /// averages about `cfg.cpu_load` busy with `cfg.mem_use` of RAM assigned.
    pub fn new(cfg: &DemoConfig) -> Self {
        let (pcpus, mem) = (cfg.pcpus, cfg.mem);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15)
            | 1;
        let now = Instant::now();
        let scale = pcpus as f64 / 16.0;
        let mut rng = Rng(seed);
        let srs = SRS
            .iter()
            .map(|&(name, kind, lat_mult, knee)| SimSr {
                uuid: rng.uuid(),
                name,
                kind,
                lat_mult,
                // Bigger hosts come with bigger arrays.
                knee: knee * scale.max(0.25),
                iops: 0.0,
            })
            .collect();
        let mut s = DemoSource {
            rng,
            sim_t: 0.0,
            last: now,
            pcpus,
            mem,
            ci_scale: scale.sqrt().max(1.0),
            load_k: 1.0,
            stock: cfg.stock,
            mem_k: 1.0,
            pcpu_idle: vec![0; pcpus as usize],
            srs,
            doms: Vec::new(),
            next_id: 0,
            ci_seq: 1041,
            ci_timer: 8.0,
        };

        let io = |mut p: Profile, r: f64, w: f64, sz: f64, lat: f64, knee: f64| {
            p.rd_iops = r;
            p.wr_iops = w;
            p.io_size = sz;
            p.lat_us = lat;
            p.iops_knee = knee;
            p
        };
        let net = |mut p: Profile, rx: f64, tx: f64| {
            p.rx_bps = rx;
            p.tx_bps = tx;
            p
        };

        struct Tpl {
            name: &'static str,
            /// Instances on the reference 16-pCPU host...
            base: f64,
            /// ...scaled by (pcpus / 16) ^ grow: 1 = linear, 0 = fixed.
            grow: f64,
            vcpus: usize,
            mem_gib: f64,
            max_gib: f64,
            prof: Profile,
            disks: u32,
            nets: u32,
        }
        let t = |name, base, grow, vcpus, mem_gib, max_gib, prof, disks, nets| Tpl {
            name,
            base,
            grow,
            vcpus,
            mem_gib,
            max_gib,
            prof,
            disks,
            nets,
        };
        let mut tpls = vec![
            t(
                "web-frontend",
                2.0,
                1.0,
                4,
                8.0,
                8.0,
                net(
                    io(prof(0.33, 0.25, 40.0, 0.08), 120.0, 60.0, 8192.0, 180.0, 30000.0),
                    24e6,
                    95e6,
                ),
                1,
                1,
            ),
            t(
                "pg-primary",
                1.0,
                1.0,
                8,
                16.0,
                16.0,
                net(
                    io(prof(0.45, 0.2, 90.0, 0.05), 3200.0, 1800.0, 8192.0, 320.0, 9000.0),
                    40e6,
                    30e6,
                ),
                2,
                1,
            ),
            t(
                "pg-replica",
                1.0,
                1.0,
                4,
                16.0,
                16.0,
                net(
                    io(prof(0.15, 0.1, 90.0, 0.02), 400.0, 1700.0, 8192.0, 450.0, 7000.0),
                    30e6,
                    2e6,
                ),
                2,
                1,
            ),
            t(
                "k8s-worker",
                3.0,
                1.0,
                6,
                12.0,
                12.0,
                net(
                    io(prof(0.5, 0.3, 67.0, 0.1), 300.0, 500.0, 32768.0, 400.0, 6000.0),
                    60e6,
                    45e6,
                ),
                1,
                2,
            ),
            t(
                "win2022-ad",
                1.0,
                0.5,
                2,
                4.0,
                6.0,
                net(
                    io(prof(0.04, 0.03, 300.0, 0.01), 15.0, 25.0, 4096.0, 600.0, 3000.0),
                    0.3e6,
                    0.4e6,
                ),
                1,
                1,
            ),
            t(
                "backup-proxy",
                1.0,
                0.5,
                2,
                2.0,
                2.0,
                net(
                    io(prof(0.2, 0.2, 180.0, 0.02), 900.0, 0.0, 1048576.0, 2500.0, 900.0),
                    0.5e6,
                    180e6,
                ),
                1,
                1,
            ),
            t(
                "IO-lab-Debian13",
                1.0,
                0.0,
                2,
                2.0,
                2.0,
                net(
                    io(
                        prof(0.25, 0.25, 30.0, 0.15),
                        9000.0,
                        9000.0,
                        4096.0,
                        55.0,
                        60000.0,
                    ),
                    0.1e6,
                    0.1e6,
                ),
                3,
                1,
            ),
        ];
        // Big iron gets big-iron workloads.
        if pcpus >= 64 {
            tpls.push(t(
                "ml-train",
                0.5,
                1.0,
                16,
                64.0,
                64.0,
                net(
                    io(
                        prof(1.2, 0.2, 240.0, 0.02),
                        2500.0,
                        150.0,
                        1048576.0,
                        900.0,
                        4000.0,
                    ),
                    150e6,
                    10e6,
                ),
                1,
                1,
            ));
        }
        if mem >= 512 * GIB {
            tpls.push(t(
                "hana-db",
                0.0,
                0.0,
                32,
                256.0,
                256.0,
                net(
                    io(
                        prof(0.4, 0.3, 120.0, 0.05),
                        6000.0,
                        4000.0,
                        65536.0,
                        250.0,
                        25000.0,
                    ),
                    80e6,
                    60e6,
                ),
                4,
                2,
            ));
        }

        let fixed = |tp: &Tpl| tp.name == "hana-db";
        let count = |tp: &Tpl| -> usize {
            if tp.name == "hana-db" {
                // One per TiB: big, but leaves most RAM to the fleet.
                return (mem / (1024 * GIB)).max(1) as usize;
            }
            // Fixed templates always exist; scaled ones may round to none
            // on hosts smaller than the 16-pCPU reference.
            let n = (tp.base * scale.powf(tp.grow)).round() as usize;
            n.max((tp.grow == 0.0) as usize)
        };

        // Dom0 grows a little with the host, like XCP-ng's defaults.
        let dom0_vcpus = (pcpus as usize / 8).clamp(4, 16);
        let dom0_mem = nice_gib((mem as f64 / GIB as f64 * 0.01 + 2.0).clamp(2.0, 16.0));
        // Fit the fleet into the memory target; fixed-size VMs (and the
        // paused 1 GiB sandbox) come off the budget first.
        let gib = |b: u64| b as f64 / GIB as f64;
        let sized = |f: bool| -> f64 {
            tpls.iter()
                .filter(|tp| fixed(tp) == f)
                .map(|tp| tp.mem_gib * count(tp) as f64)
                .sum()
        };
        let budget = gib(mem) * cfg.mem_use - gib(dom0_mem) - sized(true) - 1.0;
        let mem_k = (budget / sized(false).max(1.0)).clamp(0.05, 8.0);
        s.mem_k = mem_k;

        s.add(
            "Domain-0",
            dom0_vcpus,
            dom0_mem,
            dom0_mem,
            prof(0.10, 0.05, 23.0, 0.05),
            0,
            0,
        );
        for tp in &tpls {
            let n = count(tp);
            for i in 1..=n {
                let name = match (n, tp.name) {
                    (1, name) => name.to_string(),
                    (_, "web-frontend") => format!("{}-{i:02}", tp.name),
                    (_, name) => format!("{name}-{i}"),
                };
                let mut p = tp.prof.clone();
                // Siblings shouldn't move in lockstep.
                p.cpu *= s.rng.range(0.75, 1.25);
                p.period *= s.rng.range(0.8, 1.3);
                let (m, mx) = if fixed(tp) {
                    (nice_gib(tp.mem_gib), nice_gib(tp.max_gib))
                } else {
                    (nice_gib(tp.mem_gib * mem_k), nice_gib(tp.max_gib * mem_k))
                };
                s.add(&name, tp.vcpus, m, mx.max(m), p, tp.disks, tp.nets);
            }
        }
        s.add("sandbox-old", 1, GIB, GIB, prof(0.0, 0.0, 1.0, 0.0), 1, 1);
        if let Some(d) = s.doms.last_mut() {
            d.paused = true;
        }

        // Normalise guest load so the host averages the requested busy
        // fraction however many vCPUs it ended up with. Per vCPU: the base
        // level (vCPUs above vCPU0 run ~17% lighter, see advance()), plus
        // bursts (mean 0.4, ~3 s decay, burst_p per second), plus the bias
        // of clamping ±0.08 jitter at zero. CI jobs average ~1 running.
        let per_vcpu = |p: &Profile| (p.cpu + p.burst_p * 0.4 * 3.0) * 0.85 + 0.02;
        let demand: f64 = s.doms[1..]
            .iter()
            .filter(|d| !d.paused)
            .map(|d| per_vcpu(&d.prof) * d.vcpus.len() as f64)
            .sum::<f64>()
            + 4.0 * per_vcpu(&prof(0.85, 0.1, 10.0, 0.3)) * s.ci_scale * 0.6;
        let dom0 = per_vcpu(&s.doms[0].prof) * s.doms[0].vcpus.len() as f64;
        let target = (pcpus as f64 * cfg.cpu_load - dom0).max(0.1);
        let k = if demand > 0.0 { target / demand } else { 1.0 };
        s.load_k = k;
        for d in &mut s.doms[1..] {
            d.prof.cpu = (d.prof.cpu * k).min(0.95);
            d.prof.swing = (d.prof.swing * k).min(0.5);
        }
        // The estimate above ignores clamping, pCPU saturation and the like:
        // run the host forward and correct until the measured load hits the
        // target. CI jobs are paused meanwhile and budgeted for separately.
        let ci_jobs = 0.75 * s.ci_scale;
        for _ in 0..4 {
            let ci = ci_jobs * s.ci_vcpus() as f64 * 0.85 * 0.55 * 0.85 * s.load_k;
            let want = (pcpus as f64 * cfg.cpu_load - ci).max(0.05 * pcpus as f64);
            let got = s.measure_load(30) * pcpus as f64;
            let k = ((want - dom0) / (got - dom0).max(0.01)).clamp(0.2, 5.0);
            if (k - 1.0).abs() < 0.02 {
                break;
            }
            s.scale_load(k);
        }
        s
    }

    /// Run the simulation `secs` seconds (no CI jobs) and return the mean
    /// host busy fraction.
    fn measure_load(&mut self, secs: usize) -> f64 {
        let idle0: u64 = self.pcpu_idle.iter().sum();
        let ci = std::mem::replace(&mut self.ci_timer, f64::INFINITY);
        for _ in 0..secs {
            self.advance(1.0);
        }
        self.ci_timer = ci;
        let idle1: u64 = self.pcpu_idle.iter().sum();
        1.0 - (idle1 - idle0) as f64 / (self.pcpus as f64 * secs as f64 * 1e9)
    }

    fn ci_vcpus(&self) -> usize {
        (self.pcpus as usize / 4).clamp(1, 4)
    }

    fn scale_load(&mut self, k: f64) {
        self.load_k *= k;
        for d in &mut self.doms[1..] {
            d.prof.cpu = (d.prof.cpu * k).min(0.95);
            d.prof.swing = (d.prof.swing * k).min(0.5);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        name: &str,
        vcpus: usize,
        mem: u64,
        max_mem: u64,
        prof: Profile,
        disks: u32,
        nets: u32,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let mut shares: Vec<f64> = (0..disks)
            .map(|i| if i == 0 { 1.0 } else { 0.6 / i as f64 })
            .collect();
        let total: f64 = shares.iter().sum();
        shares.iter_mut().for_each(|s| *s /= total);
        let phase = self.rng.range(0.0, TAU);
        let uuid = self.rng.uuid();
        // XAPI publishes a balloon target for every guest; a couple are
        // being squeezed (or grown back) right now.
        let mem_target = match name {
            "Domain-0" => None,
            "win2022-ad" => Some(mem * 3 / 4),
            "k8s-worker-2" | "pg-replica" => Some(mem - mem / 6),
            _ => Some(mem),
        };
        self.doms.push(SimDom {
            id,
            name: name.into(),
            uuid,
            mem,
            max_mem,
            mem_target,
            prof,
            phase,
            burst: 0.0,
            vcpu_util: vec![0.0; vcpus],
            vcpus: vec![
                VcpuRaw {
                    online: true,
                    ns: 0,
                    runnable_ns: Some(0),
                    runstate_at_ns: None,
                };
                vcpus
            ],
            disks: shares
                .into_iter()
                .enumerate()
                .map(|(i, share)| {
                    let dev = 51712 + 16 * i as u32;
                    let sr = sr_for(name, i);
                    let backing = Backing {
                        sr: Some(self.srs[sr].uuid.clone()),
                        vdi: Some(self.rng.uuid()),
                        sr_kind: Some(self.srs[sr].kind.into()),
                        // What xapi would add on an XCP-ng host.
                        sr_name: Some(self.srs[sr].name.into()),
                        vdi_name: Some(format!("{name} {i}")),
                        path: None,
                    };
                    let kind = if id == 0 { VbdKind::Blkback } else { VbdKind::Vbd3 };
                    SimDisk {
                        kind,
                        share,
                        raw: VbdRaw {
                            dev,
                            kind,
                            oo_reqs: 0,
                            rd_reqs: 0,
                            wr_reqs: 0,
                            rd_sects: 0,
                            wr_sects: 0,
                            error: false,
                            connecting: false,
                            ext: None,
                            backing: Some(backing),
                        },
                        ext: VbdExt::default(),
                        sr,
                    }
                })
                .collect(),
            nets: (0..nets)
                .map(|i| NetRaw {
                    id: i,
                    network: Some(if i == 0 { "VM network" } else { "Storage network" }.into()),
                    ..Default::default()
                })
                .collect(),
            paused: false,
            ttl: None,
        });
    }

    fn advance(&mut self, dt: f64) {
        self.sim_t += dt;
        let t = self.sim_t;

        // CI jobs: spawn every so often, live for a while, then vanish.
        self.ci_timer -= dt;
        if self.ci_timer <= 0.0 {
            self.ci_timer = self.rng.range(25.0, 55.0) / self.ci_scale;
            let seq = self.ci_seq;
            self.ci_seq += 1;
            let mut p = prof(0.85, 0.1, 10.0, 0.3);
            p.cpu = (p.cpu * self.load_k).min(0.95);
            p.swing *= self.load_k;
            p.rd_iops = 1500.0;
            p.wr_iops = 2500.0;
            p.io_size = 65536.0;
            p.lat_us = 350.0;
            p.iops_knee = 5000.0;
            p.rx_bps = 80e6;
            p.tx_bps = 5e6;
            let m = nice_gib(4.0 * self.mem_k);
            self.add(&format!("ci-job-{seq}"), self.ci_vcpus(), m, m, p, 1, 1);
            let ttl = self.rng.range(20.0, 40.0);
            if let Some(d) = self.doms.last_mut() {
                d.ttl = Some(ttl);
            }
        }
        self.doms.retain_mut(|d| match d.ttl.as_mut() {
            Some(ttl) => {
                *ttl -= dt;
                *ttl > 0.0
            }
            None => true,
        });

        let n = self.pcpus as usize;
        let mut pcpu_load = vec![0f64; n];
        let mut sr_iops = vec![0f64; self.srs.len()];
        let rot = (t / 7.0) as usize;
        // (domain index, vCPU index, home pCPU, demand) for steal time.
        let mut placed: Vec<(usize, usize, usize, f64)> = Vec::new();

        for (di, d) in self.doms.iter_mut().enumerate() {
            if d.paused {
                d.vcpu_util.iter_mut().for_each(|u| *u = 0.0);
                continue;
            }
            let p = &d.prof;
            if self.rng.f() < p.burst_p * dt {
                d.burst = self.rng.range(0.2, 0.6) * self.load_k.min(1.0);
            }
            d.burst *= (-dt / 3.0).exp();
            let wave = (TAU * t / p.period + d.phase).sin();
            let level = (p.cpu + p.swing * wave + d.burst).clamp(0.0, 1.0);
            let jitter = 0.08 * self.load_k.min(1.0);

            let spread = (d.vcpus.len().max(2) - 1) as f64;
            for (j, (u, v)) in d.vcpu_util.iter_mut().zip(&mut d.vcpus).enumerate() {
                // vCPUs aren't perfectly balanced: vCPU0 does more.
                let skew = 1.0 - 0.35 * (j as f64 / spread).min(1.0);
                let target = (level * skew + self.rng.range(-jitter, jitter)).clamp(0.0, 1.0);
                *u += (target - *u) * (1.0 - (-dt / 1.5).exp());
                v.ns += (*u * dt * 1e9) as u64;

                // Place on a pCPU, dom0 vCPU j on pCPU j, spill over on
                // saturation.
                let mut c = if d.id == 0 {
                    j % n
                } else {
                    (d.id as usize * 5 + j * 3 + rot) % n
                };
                placed.push((di, j, c, *u));
                let mut rest = *u;
                for _ in 0..n {
                    let room = 1.0 - pcpu_load[c];
                    let take = rest.min(room);
                    pcpu_load[c] += take;
                    rest -= take;
                    if rest <= 1e-6 {
                        break;
                    }
                    c = (c + 1) % n;
                }
            }

            // Disk: Little's law for in-flight, latency grows with load.
            let io_level = (0.25 + 0.75 * ((level + 0.2) / (p.cpu + 0.2)).min(2.5)).max(0.0);
            for disk in &mut d.disks {
                let r = p.rd_iops * disk.share * io_level * self.rng.range(0.85, 1.15);
                let w = p.wr_iops * disk.share * io_level * self.rng.range(0.85, 1.15);
                let load = (r + w) / p.iops_knee;
                let sr = &self.srs[disk.sr];
                let sr_load = sr.iops / sr.knee;
                let lat = p.lat_us
                    * sr.lat_mult
                    * (1.0 + load * load)
                    * (1.0 + 0.6 * sr_load * sr_load)
                    * self.rng.range(0.9, 1.1);
                sr_iops[disk.sr] += r + w;
                let (nr, nw) = ((r * dt) as u64, (w * dt) as u64);
                let raw = &mut disk.raw;
                raw.rd_reqs += nr;
                raw.wr_reqs += nw;
                raw.rd_sects += (nr as f64 * p.io_size / 512.0) as u64;
                raw.wr_sects += (nw as f64 * p.io_size / 512.0) as u64;
                if load > 1.2 {
                    raw.oo_reqs += ((load - 1.2) * 50.0 * dt) as u64;
                }
                let fl_r = (r * lat / 1e6).round() as u64;
                let fl_w = (w * lat / 1e6).round() as u64;
                let e = &mut disk.ext;
                e.rd_done = raw.rd_reqs.saturating_sub(fl_r).max(e.rd_done);
                e.wr_done = raw.wr_reqs.saturating_sub(fl_w).max(e.wr_done);
                e.rd_usecs += (nr as f64 * lat * 0.85) as u64;
                e.wr_usecs += (nw as f64 * lat * 1.15) as u64;
                if disk.kind == VbdKind::Vbd3 {
                    raw.ext = Some(*e);
                }
            }

            for net in &mut d.nets {
                let f = io_level * self.rng.range(0.7, 1.3);
                let rx = p.rx_bps * f * dt;
                let tx = p.tx_bps * f * dt;
                // Backend view: guest TX is what dom0 receives.
                net.rbytes += tx as u64;
                net.tbytes += rx as u64;
                net.rpackets += (tx / 900.0) as u64;
                net.tpackets += (rx / 1200.0) as u64;
            }
        }

        let k = 1.0 - (-dt / 2.0).exp();
        for (sr, iops) in self.srs.iter_mut().zip(sr_iops) {
            sr.iops += (iops - sr.iops) * k;
        }

        // Steal time: a vCPU waits for its pCPU in proportion to how much it
        // wants to run and how contended that pCPU is (sharply so near
        // saturation), plus a host-wide term once the pCPUs are
        // oversubscribed. Dom0 gets scheduling priority and waits less.
        let demand: f64 = placed.iter().map(|p| p.3).sum();
        let over = (demand / n.max(1) as f64 - 0.85).max(0.0) * 1.5;
        for (di, j, c, u) in placed {
            let l = pcpu_load[c].min(1.0);
            let k = if self.doms[di].id == 0 { 0.3 } else { 1.0 };
            let frac = (u * (0.005 + 0.12 * l.powi(4) + over) * k * self.rng.range(0.7, 1.3)).min(1.0 - u);
            if let Some(r) = self.doms[di].vcpus[j].runnable_ns.as_mut() {
                *r += (frac.max(0.0) * dt * 1e9) as u64;
            }
        }

        for (idle, load) in self.pcpu_idle.iter_mut().zip(&pcpu_load) {
            let busy = (load + self.rng.range(0.0, 0.02)).min(1.0);
            *idle += ((1.0 - busy) * dt * 1e9) as u64;
        }
    }
}

impl Source for DemoSource {
    fn sample(&mut self) -> Result<Snapshot> {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.advance(dt);
        Ok(self.snapshot(now))
    }

    fn describe(&self) -> String {
        if self.stock {
            "demo (simulated host, stock libxenstat)".into()
        } else {
            "demo (simulated host)".into()
        }
    }

    fn status(&self) -> DataStatus {
        let m = if self.stock { Avail::Missing } else { Avail::Lib };
        DataStatus {
            pcpu: m,
            vbd_latency: m,
            vifs: Avail::Lib,
            // Simulated xenstore; independent of the libxenstat flavour.
            storage: Avail::Fallback,
            steal: m,
            xapi: XapiState::Connected,
            vbd_latency_coverage: super::gaps::latency_coverage(&self.snapshot(self.last)),
            steal_coverage: super::gaps::steal_coverage(&self.snapshot(self.last)),
        }
    }

    fn warmup(&mut self, secs: u32) -> Vec<Snapshot> {
        let now = Instant::now();
        let Some(start) = now.checked_sub(Duration::from_secs(secs as u64)) else {
            return Vec::new();
        };
        let snaps = (0..secs)
            .map(|i| {
                self.advance(1.0);
                self.snapshot(start + Duration::from_secs(i as u64))
            })
            .collect();
        self.last = start + Duration::from_secs(secs.saturating_sub(1) as u64);
        snaps
    }
}

impl DemoSource {
    fn snapshot(&self, at: Instant) -> Snapshot {
        let used: u64 = self.doms.iter().map(|d| d.mem).sum::<u64>() + 512 * MIB;
        let tot = self.mem;
        Snapshot {
            at,
            hostname: "xcp-ng-demo".into(),
            xen_version: "4.17.6-demo".into(),
            num_cpus: self.pcpus,
            cpu_hz: 3_600_000_000,
            tot_mem: tot,
            free_mem: tot.saturating_sub(used),
            pcpu_idle_ns: (!self.stock).then(|| {
                self.pcpu_idle
                    .iter()
                    .enumerate()
                    .map(|(i, &ns)| (i as u32, ns))
                    .collect()
            }),
            domains: self
                .doms
                .iter()
                .map(|d| DomainRaw {
                    id: d.id,
                    name: d.name.clone(),
                    state: if d.paused {
                        DomState::Paused
                    } else if d.vcpu_util.iter().any(|&u| u > 0.5) {
                        DomState::Running
                    } else {
                        DomState::Blocked
                    },
                    flags: if d.paused {
                        flag::PAUSED | flag::BLOCKED
                    } else if d.vcpu_util.iter().any(|&u| u > 0.5) {
                        flag::RUNNING
                    } else {
                        flag::BLOCKED
                    },
                    ssid: 0,
                    cpu_ns: d.vcpus.iter().map(|v| v.ns).sum(),
                    vcpus: d
                        .vcpus
                        .iter()
                        .map(|v| VcpuRaw {
                            runnable_ns: if self.stock { None } else { v.runnable_ns },
                            ..*v
                        })
                        .collect(),
                    runnable_ns: None,
                    cur_mem: d.mem,
                    max_mem: d.max_mem,
                    nets: d.nets.clone(),
                    vbds: d
                        .disks
                        .iter()
                        .map(|x| VbdRaw {
                            ext: if self.stock { None } else { x.raw.ext },
                            ..x.raw.clone()
                        })
                        .collect(),
                    vm_uuid: Some(d.uuid.clone()),
                    mem_target: d.mem_target,
                })
                .collect(),
            host_srs: Vec::new(),
        }
    }
}
