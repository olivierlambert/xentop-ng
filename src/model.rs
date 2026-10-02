//! Raw counter snapshots as delivered by a data source, and the per-interval
//! "rates" view derived from two consecutive snapshots.

use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;

/// One sample of the whole host. All counters are cumulative.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub at: Instant,
    pub hostname: String,
    pub xen_version: String,
    pub num_cpus: u32,
    pub cpu_hz: u64,
    pub tot_mem: u64,
    pub free_mem: u64,
    /// Cumulative idle time per online physical CPU, as (cpu id, ns).
    /// `None` when the loaded libxenstat lacks `xenstat_node_pcpu_idle_ns`.
    pub pcpu_idle_ns: Option<Vec<(u32, u64)>>,
    pub domains: Vec<DomainRaw>,
    /// SRs plugged into this host (xapi only), listed even when no VM
    /// disk on them is active.
    pub host_srs: Vec<HostSr>,
}

/// A storage repository plugged into this host, as xapi reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSr {
    pub uuid: String,
    pub name: Option<String>,
    /// SR type ("ext", "nfs", "lvmoiscsi"...).
    pub kind: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DomState {
    Running,
    Blocked,
    Paused,
    Shutdown,
    Crashed,
    Dying,
}

impl DomState {
    pub fn label(self) -> &'static str {
        match self {
            DomState::Running => "run",
            DomState::Blocked => "idle",
            DomState::Paused => "pause",
            DomState::Shutdown => "shut",
            DomState::Crashed => "CRASH",
            DomState::Dying => "dying",
        }
    }
}

/// libxenstat's individual domain state flags, for `DomainRaw::flags`.
pub mod flag {
    pub const DYING: u8 = 1 << 0;
    pub const SHUTDOWN: u8 = 1 << 1;
    pub const BLOCKED: u8 = 1 << 2;
    pub const CRASHED: u8 = 1 << 3;
    pub const PAUSED: u8 = 1 << 4;
    pub const RUNNING: u8 = 1 << 5;
}

#[derive(Clone, Debug)]
pub struct DomainRaw {
    pub id: u32,
    pub name: String,
    pub state: DomState,
    /// Raw state flags (`flag::*`); several can be set at once, e.g. paused
    /// and blocked. `state` is the summary used everywhere else.
    pub flags: u8,
    /// XSM security id; 0 without XSM.
    pub ssid: u32,
    pub cpu_ns: u64,
    pub vcpus: Vec<VcpuRaw>,
    pub cur_mem: u64,
    pub max_mem: u64,
    pub nets: Vec<NetRaw>,
    pub vbds: Vec<VbdRaw>,
    /// XAPI VM UUID, from xenstore (`/local/domain/<id>/vm`).
    pub vm_uuid: Option<String>,
    /// Balloon target in bytes (xenstore `memory/target`).
    pub mem_target: Option<u64>,
    /// Cumulative runnable time summed over all vCPUs, when only a
    /// domain-wide figure is available (XCP-ng's whole-domain runstate
    /// domctl). Per-vCPU figures in `vcpus` take precedence.
    pub runnable_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct VcpuRaw {
    pub online: bool,
    /// Cumulative running time.
    pub ns: u64,
    /// Cumulative time runnable but not running, i.e. waiting for a pCPU
    /// (steal time). Needs a hypervisor with XEN_DOMCTL_get_vcpu_runstate.
    pub runnable_ns: Option<u64>,
    /// Hypervisor system time `runnable_ns` runs to, from the same
    /// snapshot. Steal is computed over its difference rather than the
    /// snapshot's wall clock: this process can be descheduled between the
    /// hypercall and reading its own clock, exactly when the host is
    /// contended. `None` with an older libxenstat or hypervisor.
    pub runstate_at_ns: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct NetRaw {
    pub id: u32,
    /// Name of the network the VIF is on (xapi only).
    pub network: Option<String>,
    pub rbytes: u64,
    pub rpackets: u64,
    pub rerrs: u64,
    pub rdrop: u64,
    pub tbytes: u64,
    pub tpackets: u64,
    pub terrs: u64,
    pub tdrop: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VbdKind {
    Blkback,
    Tap,
    Vbd3,
    Qdisk,
    Unknown,
}

impl VbdKind {
    pub fn from_xenstat(t: u32) -> Self {
        match t {
            1 => VbdKind::Blkback,
            2 => VbdKind::Tap,
            3 => VbdKind::Vbd3,
            4 => VbdKind::Qdisk,
            _ => VbdKind::Unknown,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            VbdKind::Blkback => "blkback",
            VbdKind::Tap => "tap",
            VbdKind::Vbd3 => "tapdisk3",
            VbdKind::Qdisk => "qdisk",
            VbdKind::Unknown => "?",
        }
    }
}

#[derive(Clone, Debug)]
pub struct VbdRaw {
    pub dev: u32,
    pub kind: VbdKind,
    pub oo_reqs: u64,
    pub rd_reqs: u64,
    pub wr_reqs: u64,
    pub rd_sects: u64,
    pub wr_sects: u64,
    pub error: bool,
    /// The backend has not connected yet (e.g. a booting guest), so there
    /// are no stats to read: pending, not a collection failure.
    pub connecting: bool,
    pub ext: Option<VbdExt>,
    /// What the disk is backed by, from xenstore; `None` when unknown.
    pub backing: Option<Backing>,
}

/// Where a VBD's data lives: an XCP-ng SR/VDI pair, or a plain path on
/// other Xen hosts. All strings are sanitised and bounded by the source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Backing {
    /// Storage repository UUID.
    pub sr: Option<String>,
    /// Virtual disk image UUID.
    pub vdi: Option<String>,
    /// SR flavour: "ext", "nfs", "lvm"... worked out locally, or the SR
    /// type from xapi when there is one ("lvmoiscsi").
    pub sr_kind: Option<String>,
    /// SR and VDI name-labels (xapi only).
    pub sr_name: Option<String>,
    pub vdi_name: Option<String>,
    /// Backing path when there is no SR (plain blkback/qdisk), or for SR
    /// files that are not VDIs (ISOs).
    pub path: Option<String>,
}

impl Backing {
    /// What disks are grouped by for per-storage totals: the SR, or else
    /// the directory holding the backing file/device.
    pub fn group(&self) -> Option<String> {
        if let Some(sr) = &self.sr {
            return Some(sr.clone());
        }
        let p = self.path.as_deref()?;
        let (dir, _) = p.rsplit_once('/')?;
        Some(if dir.is_empty() { "/".into() } else { dir.into() })
    }
}

/// Extended per-VBD counters (tapdisk3 only, patched libxenstat).
#[derive(Clone, Copy, Debug, Default)]
pub struct VbdExt {
    pub rd_done: u64,
    pub wr_done: u64,
    pub rd_usecs: u64,
    pub wr_usecs: u64,
    pub io_errors: u64,
}

// ---------------------------------------------------------------------------
// Rates

/// Measurement coverage: sums include only the available devices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub available: usize,
    pub total: usize,
    /// Newly observed, healthy devices still establishing a baseline.
    pub pending: usize,
}
impl Coverage {
    pub fn complete(self) -> bool {
        self.available == self.total
    }
    pub fn has_value(self) -> bool {
        self.available > 0 || (self.total == 0 && self.pending == 0)
    }
    fn record(&mut self, valid: bool, pending: bool) {
        if pending {
            self.pending += 1;
        } else {
            self.total += 1;
            self.available += usize::from(valid);
        }
    }
    pub fn label(self, value: String) -> String {
        if !self.has_value() {
            "-".into()
        } else if !self.complete() {
            format!("{value}*")
        } else {
            value
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HostRates {
    pub disk_samples: Coverage,
    pub hostname: String,
    pub xen_version: String,
    pub num_cpus: u32,
    pub cpu_mhz: u64,
    /// Per-pCPU busy fraction (0..1); empty when unavailable.
    pub pcpu_busy: Vec<Option<f64>>,
    pub pcpu_samples: Coverage,
    /// Hypervisor CPU id for each entry of `pcpu_busy`.
    pub pcpu_ids: Vec<u32>,
    /// Whole-host busy fraction (0..1).
    pub cpu_busy: f64,
    /// True if `cpu_busy` is estimated from domain CPU time rather than
    /// measured from pCPU idle counters.
    pub cpu_estimated: bool,
    pub mem_total: u64,
    pub mem_free: u64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub disk_rd_bps: f64,
    pub disk_wr_bps: f64,
    pub disk_rd_iops: f64,
    pub disk_wr_iops: f64,
    /// Request-weighted mean service latency across all extended VBDs (µs).
    pub disk_rd_lat_us: Option<f64>,
    pub disk_wr_lat_us: Option<f64>,
    /// Share of wall time vCPUs spent runnable but not running, averaged
    /// over every vCPU that reports it (same definition as `DomRates`).
    /// `None` when no domain reports steal time.
    pub steal_pct: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DomRates {
    pub disk_samples: Coverage,
    /// No compatible preceding domain sample; start history afresh.
    pub baseline_reset: bool,
    pub id: u32,
    pub name: String,
    pub state: Option<DomState>,
    /// 100.0 == one physical CPU fully used.
    pub cpu_pct: f64,
    pub vcpu_pct: Vec<f64>,
    /// Steal time: share of the interval the domain's online vCPUs spent
    /// runnable but not running (waiting for a pCPU), averaged over them,
    /// like `st` in a Linux guest's top. 0..100.
    pub steal_pct: Option<f64>,
    /// Steal time per vCPU (0..100), when the hypervisor reports it per vCPU.
    pub vcpu_steal_pct: Vec<Option<f64>>,
    pub vcpus_online: usize,
    pub mem: u64,
    pub max_mem: u64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub net_errs: u64,
    pub net_drops: u64,
    pub disk_rd_bps: f64,
    pub disk_wr_bps: f64,
    pub disk_rd_iops: f64,
    pub disk_wr_iops: f64,
    pub disk_rd_lat_us: Option<f64>,
    pub disk_wr_lat_us: Option<f64>,
    pub disk_oo_ps: f64,
    pub disk_errors: u64,
    pub vbds: Vec<VbdRates>,
    pub nets: Vec<NetRates>,
    pub vm_uuid: Option<String>,
    /// Balloon target (bytes), when the toolstack publishes one.
    pub mem_target: Option<u64>,
}

impl DomRates {
    pub fn disk_bps(&self) -> f64 {
        self.disk_rd_bps + self.disk_wr_bps
    }
    pub fn net_bps(&self) -> f64 {
        self.net_rx_bps + self.net_tx_bps
    }
    /// Worst of read/write latency, for sorting and colouring.
    pub fn lat_us(&self) -> Option<f64> {
        match (self.disk_rd_lat_us, self.disk_wr_lat_us) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VbdRates {
    /// Two successful, monotonic counter samples from this device.
    pub stats_valid: bool,
    /// A healthy new/replaced disk needs a baseline, not an error marker.
    pub warming_up: bool,
    /// Collection failure, distinct from a disk-reported I/O error.
    pub collection_error: bool,
    pub dev: u32,
    pub name: String,
    pub kind: Option<VbdKind>,
    pub rd_bps: f64,
    pub wr_bps: f64,
    pub rd_iops: f64,
    pub wr_iops: f64,
    pub rd_lat_us: Option<f64>,
    pub wr_lat_us: Option<f64>,
    pub oo_ps: f64,
    pub errors: u64,
    #[serde(flatten)]
    pub backing: Backing,
}

/// Totals for one storage repository (or backing directory), over every
/// VBD on it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SrRates {
    pub disk_samples: Coverage,
    /// SR UUID, or the backing directory when there is no SR.
    pub sr: String,
    /// SR name-label (xapi only).
    pub name: Option<String>,
    pub kind: Option<String>,
    pub vbds: usize,
    pub rd_bps: f64,
    pub wr_bps: f64,
    pub rd_iops: f64,
    pub wr_iops: f64,
    /// Request-weighted mean service latency (µs), tapdisk3 VBDs only.
    pub rd_lat_us: Option<f64>,
    pub wr_lat_us: Option<f64>,
    /// Domain issuing the most IOPS on this SR.
    pub top_id: Option<u32>,
    pub top_name: Option<String>,
    pub top_iops: f64,
}

impl SrRates {
    pub fn iops(&self) -> f64 {
        self.rd_iops + self.wr_iops
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NetRates {
    pub id: u32,
    pub network: Option<String>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_pps: f64,
    pub tx_pps: f64,
    pub errs: u64,
    pub drops: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Rates {
    pub interval_s: f64,
    pub host: HostRates,
    pub domains: Vec<DomRates>,
    /// Per-SR totals, busiest first. Empty without storage mapping.
    pub srs: Vec<SrRates>,
}

/// Running sums for one SR while computing rates.
#[derive(Default)]
struct SrAcc {
    r: SrRates,
    rd_us: u64,
    wr_us: u64,
    rd_done: u64,
    wr_done: u64,
    /// IOPS per domain on this SR.
    per_dom: HashMap<u32, f64>,
}

/// Fold per-SR sums into the final, busiest-first list.
fn finish_srs(acc: HashMap<String, SrAcc>, domains: &[DomRates]) -> Vec<SrRates> {
    let names: HashMap<u32, &str> = domains.iter().map(|d| (d.id, d.name.as_str())).collect();
    let mut v: Vec<SrRates> = acc
        .into_iter()
        .map(|(sr, a)| {
            let mut r = a.r;
            r.sr = sr;
            r.rd_lat_us = lat(a.rd_us, a.rd_done);
            r.wr_lat_us = lat(a.wr_us, a.wr_done);
            // Ties go to the lowest domid so the pick doesn't flicker.
            if let Some((&id, &iops)) = a
                .per_dom
                .iter()
                .max_by(|x, y| x.1.total_cmp(y.1).then(y.0.cmp(x.0)))
            {
                r.top_id = Some(id);
                r.top_name = names.get(&id).map(|n| n.to_string());
                r.top_iops = iops;
            }
            r
        })
        .collect();
    v.sort_by(|a, b| b.iops().total_cmp(&a.iops()).then_with(|| a.sr.cmp(&b.sr)));
    v
}

/// Counter delta that survives resets (domain reboot, device replug).
fn d(cur: u64, prev: u64) -> u64 {
    cur.saturating_sub(prev)
}

fn lat(usecs: u64, reqs: u64) -> Option<f64> {
    (reqs > 0).then(|| usecs as f64 / reqs as f64)
}

/// Share of the interval each vCPU spent runnable (0..1), or `None` for
/// vCPUs without runstate data in either sample. Measured against the
/// hypervisor's own snapshot times when both samples have them, so the
/// figure doesn't depend on when this process got to read its clock;
/// otherwise against the wall-clock interval `dt_ns`.
fn vcpu_runnable(cur: &DomainRaw, prev: &DomainRaw, dt_ns: f64) -> Vec<Option<f64>> {
    cur.vcpus
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let p = prev.vcpus.get(i)?;
            let ran = d(v.runnable_ns?, p.runnable_ns?) as f64;
            let span = match (v.runstate_at_ns, p.runstate_at_ns) {
                (Some(c), Some(p)) if c > p => (c - p) as f64,
                _ => dt_ns,
            };
            Some((ran / span).min(1.0))
        })
        .collect()
}

/// vCPUs' worth of runnable time per unit of time for the whole domain:
/// the sum of its vCPUs' shares when every vCPU has data, else the
/// domain-wide counter over the wall-clock interval.
fn dom_runnable(cur: &DomainRaw, prev: &DomainRaw, per_vcpu: &[Option<f64>], dt_ns: f64) -> Option<f64> {
    if !per_vcpu.is_empty() && per_vcpu.iter().all(Option::is_some) {
        return Some(per_vcpu.iter().flatten().sum());
    }
    Some(d(cur.runnable_ns?, prev.runnable_ns?) as f64 / dt_ns)
}

/// Linux-style disk name for a Xen virtual block device number.
pub fn vbd_name(dev: u32) -> String {
    let (major, minor) = (dev >> 8, dev & 0xff);
    let letters = |n: u32| -> String {
        let mut n = n;
        let mut s = Vec::new();
        loop {
            s.push(b'a' + (n % 26) as u8);
            if n < 26 {
                break;
            }
            n = n / 26 - 1;
        }
        s.reverse();
        String::from_utf8(s).unwrap_or_default()
    };
    if dev & (1 << 28) != 0 {
        // Extended scheme: 1 << 28 | disk << 8 | partition
        let disk = (dev >> 8) & 0xfffff;
        return format!("xvd{}", letters(disk));
    }
    // Name disks the way the guest's PV driver does (Linux xen-blkfront's
    // xen_translate_vdev(); Windows PV drivers number them the same way),
    // since that's what shows up in the guest's iostat. HVM disks carry
    // emulated IDE/SCSI numbers so the BIOS can boot from them: on XCP-ng
    // the first four are 768/832/5632/5696 (hda..hdd), which the guest
    // sees as xvda..xvdd.
    let index = match major {
        202 => minor >> 4,
        3 => minor >> 6,
        22 => 2 + (minor >> 6),
        8 => minor >> 4,
        65..=71 => (minor >> 4) + (major - 65 + 1) * 16,
        _ => return format!("{dev}"),
    };
    format!("xvd{}", letters(index))
}

/// UUID is a Xen/xenstore identity, independent of XAPI. Counter rollback
/// also invalidates a run, since a reboot can retain the VM's UUID.
fn same_domain(prev: &DomainRaw, cur: &DomainRaw) -> bool {
    let identity = match (&prev.vm_uuid, &cur.vm_uuid) {
        (Some(a), Some(b)) => a == b,
        (None, None) => prev.name == cur.name,
        _ => false, // Metadata disappeared or became available: rebaseline.
    };
    identity && cur.cpu_ns >= prev.cpu_ns && cur.vcpus.iter().zip(&prev.vcpus).all(|(c, p)| c.ns >= p.ns)
}

fn same_backing(a: &Option<Backing>, b: &Option<Backing>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.sr == b.sr && a.vdi == b.vdi && a.path == b.path,
        (None, None) => true,
        _ => false,
    }
}

/// Never derive rates from failed reads or across counter resets.
fn valid_vbd_pair(prev: &VbdRaw, cur: &VbdRaw) -> bool {
    !prev.error
        && !cur.error
        && !prev.connecting
        && !cur.connecting
        && same_backing(&prev.backing, &cur.backing)
        && cur.rd_reqs >= prev.rd_reqs
        && cur.wr_reqs >= prev.wr_reqs
        && cur.rd_sects >= prev.rd_sects
        && cur.wr_sects >= prev.wr_sects
        && cur.oo_reqs >= prev.oo_reqs
}

pub fn compute(prev: &Snapshot, cur: &Snapshot) -> Rates {
    let dt = cur.at.duration_since(prev.at).as_secs_f64().max(1e-3);
    let dt_ns = dt * 1e9;

    let prev_doms: HashMap<u32, &DomainRaw> = prev.domains.iter().map(|d| (d.id, d)).collect();

    let mut host = HostRates {
        hostname: cur.hostname.clone(),
        xen_version: cur.xen_version.clone(),
        num_cpus: cur.num_cpus,
        cpu_mhz: cur.cpu_hz / 1_000_000,
        mem_total: cur.tot_mem,
        mem_free: cur.free_mem,
        ..Default::default()
    };

    let (mut h_rd_us, mut h_wr_us, mut h_rd_done, mut h_wr_done) = (0u64, 0u64, 0u64, 0u64);
    let mut dom_cpu_total = 0f64;
    let mut srs: HashMap<String, SrAcc> = HashMap::new();
    // Steal over domains that report it: (runnable vCPUs, online vCPUs).
    let mut h_steal: Option<(f64, usize)> = None;

    let mut domains = Vec::with_capacity(cur.domains.len());
    for dom in &cur.domains {
        let p = prev_doms.get(&dom.id).filter(|p| same_domain(p, dom));
        let mut r = DomRates {
            baseline_reset: p.is_none(),
            id: dom.id,
            name: dom.name.clone(),
            state: Some(dom.state),
            mem: dom.cur_mem,
            max_mem: dom.max_mem,
            vm_uuid: dom.vm_uuid.clone(),
            mem_target: dom.mem_target,
            vcpus_online: dom.vcpus.iter().filter(|v| v.online).count(),
            ..Default::default()
        };

        if let Some(p) = p {
            r.cpu_pct = d(dom.cpu_ns, p.cpu_ns) as f64 / dt_ns * 100.0;
            r.vcpu_pct = dom
                .vcpus
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let pv = p.vcpus.get(i).map(|v| v.ns).unwrap_or(v.ns);
                    (d(v.ns, pv) as f64 / dt_ns * 100.0).min(100.0)
                })
                .collect();
            let per_vcpu = vcpu_runnable(dom, p, dt_ns);
            r.vcpu_steal_pct = per_vcpu.iter().map(|x| x.map(|f| f * 100.0)).collect();
            if let Some(run) = dom_runnable(dom, p, &per_vcpu, dt_ns) {
                r.steal_pct = Some((run / r.vcpus_online.max(1) as f64 * 100.0).min(100.0));
                let (hr, hv) = h_steal.unwrap_or_default();
                h_steal = Some((hr + run, hv + r.vcpus_online.max(1)));
            }
        } else {
            r.vcpu_pct = vec![0.0; dom.vcpus.len()];
            r.vcpu_steal_pct = vec![None; dom.vcpus.len()];
        }
        dom_cpu_total += r.cpu_pct / 100.0;

        for n in &dom.nets {
            let pn = p.and_then(|p| p.nets.iter().find(|x| x.id == n.id));
            let mut nr = NetRates {
                id: n.id,
                network: n.network.clone(),
                errs: n.rerrs.saturating_add(n.terrs),
                drops: n.rdrop.saturating_add(n.tdrop),
                ..Default::default()
            };
            if let Some(pn) = pn {
                // xenstat reports vif counters from dom0's point of view:
                // what the backend receives is what the guest transmits.
                nr.tx_bps = d(n.rbytes, pn.rbytes) as f64 / dt;
                nr.rx_bps = d(n.tbytes, pn.tbytes) as f64 / dt;
                nr.tx_pps = d(n.rpackets, pn.rpackets) as f64 / dt;
                nr.rx_pps = d(n.tpackets, pn.tpackets) as f64 / dt;
            }
            r.net_rx_bps += nr.rx_bps;
            r.net_tx_bps += nr.tx_bps;
            r.net_errs = r.net_errs.saturating_add(nr.errs);
            r.net_drops = r.net_drops.saturating_add(nr.drops);
            r.nets.push(nr);
        }

        let (mut rd_us, mut wr_us, mut rd_done, mut wr_done) = (0u64, 0u64, 0u64, 0u64);
        for v in &dom.vbds {
            let pv = p.and_then(|p| p.vbds.iter().find(|x| x.dev == v.dev && x.kind == v.kind));
            let mut vr = VbdRates {
                dev: v.dev,
                name: vbd_name(v.dev),
                kind: Some(v.kind),
                errors: v.ext.map(|e| e.io_errors).unwrap_or(0),
                collection_error: v.error,
                warming_up: !v.error
                    && (v.connecting
                        || pv.is_none_or(|pv| pv.connecting || !same_backing(&pv.backing, &v.backing))),
                backing: v.backing.clone().unwrap_or_default(),
                ..Default::default()
            };
            // Completed requests and their service time this interval.
            let (mut ru, mut wu, mut rn, mut wn) = (0u64, 0u64, 0u64, 0u64);
            // No in-flight estimate: tapdisk counts empty flushes as
            // submitted writes but never as completed ones, so
            // submitted - completed drifts upward forever.
            if let Some(pv) = pv.filter(|pv| valid_vbd_pair(pv, v)) {
                vr.stats_valid = true;
                vr.rd_bps = d(v.rd_sects, pv.rd_sects) as f64 * 512.0 / dt;
                vr.wr_bps = d(v.wr_sects, pv.wr_sects) as f64 * 512.0 / dt;
                vr.rd_iops = d(v.rd_reqs, pv.rd_reqs) as f64 / dt;
                vr.wr_iops = d(v.wr_reqs, pv.wr_reqs) as f64 / dt;
                vr.oo_ps = d(v.oo_reqs, pv.oo_reqs) as f64 / dt;
                if let (Some(e), Some(pe)) = (v.ext, pv.ext) {
                    if e.rd_done >= pe.rd_done
                        && e.wr_done >= pe.wr_done
                        && e.rd_usecs >= pe.rd_usecs
                        && e.wr_usecs >= pe.wr_usecs
                    {
                        (ru, wu) = (d(e.rd_usecs, pe.rd_usecs), d(e.wr_usecs, pe.wr_usecs));
                        (rn, wn) = (d(e.rd_done, pe.rd_done), d(e.wr_done, pe.wr_done));
                        vr.rd_lat_us = lat(ru, rn);
                        vr.wr_lat_us = lat(wu, wn);
                        rd_us = rd_us.saturating_add(ru);
                        wr_us = wr_us.saturating_add(wu);
                        rd_done = rd_done.saturating_add(rn);
                        wr_done = wr_done.saturating_add(wn);
                    }
                }
            }
            r.disk_samples.record(vr.stats_valid, vr.warming_up);
            r.disk_rd_bps += vr.rd_bps;
            r.disk_wr_bps += vr.wr_bps;
            r.disk_rd_iops += vr.rd_iops;
            r.disk_wr_iops += vr.wr_iops;
            r.disk_oo_ps += vr.oo_ps;
            r.disk_errors = r.disk_errors.saturating_add(vr.errors);
            if let Some(key) = vr.backing.group() {
                let a = srs.entry(key).or_default();
                a.r.vbds += 1;
                a.r.disk_samples.record(vr.stats_valid, vr.warming_up);
                if a.r.kind.is_none() {
                    a.r.kind = vr.backing.sr_kind.clone();
                }
                if a.r.name.is_none() {
                    a.r.name = vr.backing.sr_name.clone();
                }
                a.r.rd_bps += vr.rd_bps;
                a.r.wr_bps += vr.wr_bps;
                a.r.rd_iops += vr.rd_iops;
                a.r.wr_iops += vr.wr_iops;
                a.rd_us = a.rd_us.saturating_add(ru);
                a.wr_us = a.wr_us.saturating_add(wu);
                a.rd_done = a.rd_done.saturating_add(rn);
                a.wr_done = a.wr_done.saturating_add(wn);
                *a.per_dom.entry(dom.id).or_default() += vr.rd_iops + vr.wr_iops;
            }
            r.vbds.push(vr);
        }
        r.disk_rd_lat_us = lat(rd_us, rd_done);
        r.disk_wr_lat_us = lat(wr_us, wr_done);

        host.net_rx_bps += r.net_rx_bps;
        host.net_tx_bps += r.net_tx_bps;
        host.disk_samples.total += r.disk_samples.total;
        host.disk_samples.pending += r.disk_samples.pending;
        host.disk_samples.available += r.disk_samples.available;
        host.disk_rd_bps += r.disk_rd_bps;
        host.disk_wr_bps += r.disk_wr_bps;
        host.disk_rd_iops += r.disk_rd_iops;
        host.disk_wr_iops += r.disk_wr_iops;
        h_rd_us = h_rd_us.saturating_add(rd_us);
        h_wr_us = h_wr_us.saturating_add(wr_us);
        h_rd_done = h_rd_done.saturating_add(rd_done);
        h_wr_done = h_wr_done.saturating_add(wr_done);
        domains.push(r);
    }
    host.disk_rd_lat_us = lat(h_rd_us, h_rd_done);
    host.disk_wr_lat_us = lat(h_wr_us, h_wr_done);
    // Same definition as the per-domain figure (and as `st` in a Linux
    // guest): share of wall time spent runnable, averaged over every vCPU
    // that reports it. A share-of-demand ratio looks dramatic on idle hosts
    // (a few ms of wake-up latency against a few ms of work) and would not
    // match the STEAL column.
    host.steal_pct = h_steal.map(|(run, vcpus)| (run / vcpus.max(1) as f64 * 100.0).min(100.0));

    match (&prev.pcpu_idle_ns, &cur.pcpu_idle_ns) {
        (Some(pi), Some(ci)) if !ci.is_empty() => {
            let prev_idle: HashMap<u32, u64> = pi.iter().copied().collect();
            for &(id, c) in ci {
                let busy = prev_idle
                    .get(&id)
                    .and_then(|&p| c.checked_sub(p))
                    .map(|idle| (1.0 - idle as f64 / dt_ns).clamp(0.0, 1.0));
                host.pcpu_ids.push(id);
                host.pcpu_busy.push(busy);
            }
            host.pcpu_samples = Coverage {
                pending: 0,
                total: host.pcpu_busy.len(),
                available: host.pcpu_busy.iter().flatten().count(),
            };
            if host.pcpu_samples.available > 0 {
                host.cpu_busy =
                    host.pcpu_busy.iter().flatten().sum::<f64>() / host.pcpu_samples.available as f64;
            } else {
                host.cpu_estimated = true;
                host.cpu_busy = (dom_cpu_total / cur.num_cpus.max(1) as f64).clamp(0.0, 1.0);
            }
        }
        _ => {
            host.cpu_estimated = true;
            host.cpu_busy = (dom_cpu_total / cur.num_cpus.max(1) as f64).clamp(0.0, 1.0);
        }
    }

    // SRs with no active disk still get a row, idle.
    for h in &cur.host_srs {
        let a = srs.entry(h.uuid.clone()).or_default();
        if h.kind.is_some() {
            a.r.kind = h.kind.clone();
        }
        if a.r.name.is_none() {
            a.r.name = h.name.clone();
        }
    }
    let srs = finish_srs(srs, &domains);
    Rates {
        interval_s: dt,
        host,
        domains,
        srs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vbd_names() {
        assert_eq!(vbd_name(51712), "xvda");
        assert_eq!(vbd_name(51728), "xvdb");
        assert_eq!(vbd_name(51808), "xvdg");
        // Emulated IDE/SCSI numbers, as the guest's PV driver names them.
        assert_eq!(vbd_name(768), "xvda"); // hda
        assert_eq!(vbd_name(832), "xvdb"); // hdb
        assert_eq!(vbd_name(5632), "xvdc"); // hdc
        assert_eq!(vbd_name(5696), "xvdd"); // hdd
        assert_eq!(vbd_name(2048), "xvda"); // sda
        assert_eq!(vbd_name(2064), "xvdb"); // sdb
        assert_eq!(vbd_name(65 << 8), "xvdq"); // sdq
        assert_eq!(vbd_name(51776), "xvde");
        assert_eq!(vbd_name((1 << 28) | (27 << 8)), "xvdab");
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use std::time::Duration;

    fn snap(at: Instant, doms: Vec<DomainRaw>) -> Snapshot {
        Snapshot {
            at,
            hostname: "h".into(),
            xen_version: "x".into(),
            num_cpus: 2,
            cpu_hz: 0,
            tot_mem: 1 << 30,
            free_mem: 0,
            pcpu_idle_ns: None,
            domains: doms,
            host_srs: Vec::new(),
        }
    }

    fn dom(id: u32, name: &str, cpu_ns: u64, rd_reqs: u64) -> DomainRaw {
        DomainRaw {
            id,
            name: name.into(),
            state: DomState::Running,
            flags: flag::RUNNING,
            ssid: 0,
            cpu_ns,
            vcpus: vec![VcpuRaw {
                online: true,
                ns: cpu_ns,
                runnable_ns: None,
                runstate_at_ns: None,
            }],
            cur_mem: 0,
            max_mem: 0,
            runnable_ns: None,
            nets: vec![],
            vbds: vec![VbdRaw {
                dev: 51712,
                kind: VbdKind::Vbd3,
                oo_reqs: 0,
                rd_reqs,
                wr_reqs: 0,
                rd_sects: 0,
                wr_sects: 0,
                error: false,
                connecting: false,
                ext: None,
                backing: None,
            }],
            vm_uuid: None,
            mem_target: None,
        }
    }

    #[test]
    fn counters_going_backwards_are_not_negative_or_huge() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // tapdisk restarted: request counter reset.
        let r = compute(
            &snap(t0, vec![dom(5, "vm", 10, 1_000_000)]),
            &snap(t1, vec![dom(5, "vm", 5, 3)]),
        );
        let d = &r.domains[0];
        assert_eq!(d.cpu_pct, 0.0);
        assert_eq!(d.disk_rd_iops, 0.0);
    }

    #[test]
    fn failed_disk_read_requires_a_new_baseline() {
        let t = Instant::now();
        let good = snap(t, vec![dom(5, "vm", 0, 1_000_000)]);
        let mut failed = snap(t + Duration::from_secs(1), vec![dom(5, "vm", 0, 0)]);
        failed.domains[0].vbds[0].error = true;
        let recovered = snap(t + Duration::from_secs(2), vec![dom(5, "vm", 0, 1_000_010)]);
        let next = snap(t + Duration::from_secs(3), vec![dom(5, "vm", 0, 1_000_020)]);
        for (a, b) in [(&good, &failed), (&failed, &recovered)] {
            let r = compute(a, b);
            assert!(!r.domains[0].vbds[0].stats_valid);
            assert_eq!(r.host.disk_rd_iops, 0.0);
            assert_eq!(r.domains[0].disk_errors, 0);
        }
        let r = compute(&recovered, &next);
        assert!(r.domains[0].vbds[0].stats_valid);
        assert_eq!(r.host.disk_rd_iops, 10.0);
    }

    #[test]
    fn booting_disk_is_pending_until_it_has_a_baseline() {
        let t = Instant::now();
        let mut connecting = snap(t, vec![dom(9, "vm", 0, 0)]);
        connecting.domains[0].vbds[0].connecting = true;
        let mut later = connecting.clone();
        later.at += Duration::from_secs(1);
        let first = snap(t + Duration::from_secs(2), vec![dom(9, "vm", 0, 5_000)]);
        let next = snap(t + Duration::from_secs(3), vec![dom(9, "vm", 0, 5_010)]);
        for (a, b) in [(&connecting, &later), (&later, &first)] {
            let r = compute(a, b);
            let v = &r.domains[0].vbds[0];
            assert!(v.warming_up && !v.stats_valid && !v.collection_error);
            assert!(r.host.disk_samples.complete(), "not partial data");
            assert_eq!(r.host.disk_rd_iops, 0.0, "no spike from a zero baseline");
        }
        let r = compute(&first, &next);
        assert!(r.domains[0].vbds[0].stats_valid);
        assert_eq!(r.host.disk_rd_iops, 10.0);
    }

    #[test]
    fn hotplug_is_pending_and_partial_graphs_keep_valid_sums() {
        let t = Instant::now();
        let mut old = dom(5, "vm", 100, 100);
        old.vbds[0].rd_sects = 800;
        old.vbds[0].backing = Some(Backing {
            sr: Some("sr".into()),
            ..Default::default()
        });
        let mut cur = old.clone();
        cur.vbds[0].rd_reqs += 10;
        cur.vbds[0].rd_sects += 80;
        let mut extra = cur.vbds[0].clone();
        extra.dev += 16;
        cur.vbds.push(extra);
        let a = snap(t, vec![old]);
        let b = snap(t + Duration::from_secs(1), vec![cur]);
        let r = compute(&a, &b);
        assert!(r.domains[0].vbds[1].warming_up);
        assert_eq!(
            r.host.disk_samples,
            Coverage {
                available: 1,
                total: 1,
                pending: 1
            }
        );
        assert!(r.host.disk_samples.complete());
        assert_eq!(r.host.disk_samples.label("10".into()), "10");
        let mut failed = b.clone();
        failed.domains[0].vbds[1].error = true;
        let r = compute(&a, &failed);
        assert_eq!(
            r.host.disk_samples,
            Coverage {
                available: 1,
                total: 2,
                pending: 0
            }
        );
        assert_eq!(r.host.disk_samples.label("10".into()), "10*");
        let mut h = crate::history::History::default();
        h.record(&r);
        assert_eq!(h.rd.tail(1), vec![40960.0]);
        assert_eq!(h.riops.tail(1), vec![10.0]);
        assert_eq!(h.doms[&5].rd.tail(1), vec![40960.0]);
        assert_eq!(h.srs["sr"].iops.back(), Some(&10.0));
        failed.domains[0].vbds[0].error = true;
        let r = compute(&a, &failed);
        h.record(&r);
        assert!(h.rd.tail(1)[0].is_nan());
        assert!(h.doms[&5].rd.tail(1)[0].is_nan());
        assert!(h.srs["sr"].iops.back().unwrap().is_nan());
        let pending = Coverage {
            available: 0,
            total: 0,
            pending: 1,
        };
        assert!(!pending.has_value());
        assert_eq!(pending.label("0".into()), "-");
        assert!(Coverage::default().has_value(), "no disks is idle, not missing");
    }

    #[test]
    fn reused_domid_starts_fresh() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let r = compute(
            &snap(t0, vec![dom(5, "old", 0, 0)]),
            &snap(t1, vec![dom(5, "new", 4_000_000_000, 9)]),
        );
        // No previous sample for "new": no bogus 400% spike.
        assert_eq!(r.domains[0].cpu_pct, 0.0);
    }

    #[test]
    fn pcpu_hotplug_and_resets_have_no_invented_utilization() {
        let t = Instant::now();
        let mut a = snap(t, vec![]);
        let mut b = snap(t + Duration::from_secs(1), vec![]);
        a.pcpu_idle_ns = Some(vec![(0, 1_000_000_000), (2, 2_000_000_000)]);
        b.pcpu_idle_ns = Some(vec![(0, 2_000_000_000), (3, 3_000_000_000)]);
        let r = compute(&a, &b);
        assert_eq!(r.host.pcpu_ids, vec![0, 3]);
        assert_eq!(r.host.pcpu_busy, vec![Some(0.0), None]);
        assert_eq!(r.host.cpu_busy, 0.0);
        assert_eq!(
            r.host.pcpu_samples,
            Coverage {
                pending: 0,
                available: 1,
                total: 2
            }
        );
        let json = serde_json::to_value(&r).unwrap();
        assert!(json["host"]["pcpu_busy"][1].is_null());
        b.pcpu_idle_ns = Some(vec![(0, 1), (2, 1)]);
        let r = compute(&a, &b);
        assert_eq!(r.host.pcpu_busy, vec![None, None]);
        assert!(r.host.cpu_estimated);
    }

    #[test]
    fn identity_without_xapi_and_rename() {
        let t = Instant::now();
        let mut a = snap(t, vec![dom(5, "old name", 100, 100)]);
        let mut b = snap(t + Duration::from_secs(1), vec![dom(5, "renamed", 200, 110)]);
        a.domains[0].vm_uuid = Some("uuid-a".into());
        b.domains[0].vm_uuid = Some("uuid-a".into());
        let r = compute(&a, &b);
        assert!(!r.domains[0].baseline_reset);
        assert_eq!(r.domains[0].disk_rd_iops, 10.0);
        let mut history = crate::history::History::default();
        history.record(&r);
        let mut renamed = r.clone();
        renamed.domains[0].name = "renamed again".into();
        history.record(&renamed);
        assert_eq!(history.doms[&5].cpu.tail(10).len(), 2);
        b.domains[0].name = a.domains[0].name.clone();
        b.domains[0].vm_uuid = Some("uuid-b".into());
        let r = compute(&a, &b);
        assert!(r.domains[0].baseline_reset);
        assert_eq!(r.domains[0].disk_rd_iops, 0.0);
        history.record(&r);
        assert_eq!(history.doms[&5].cpu.tail(10).len(), 1);
        b.domains[0].vm_uuid = Some("uuid-a".into());
        b.domains[0].cpu_ns = 1;
        assert!(compute(&a, &b).domains[0].baseline_reset);
        a.domains[0].vm_uuid = None;
        b.domains[0].vm_uuid = None;
        assert!(compute(&a, &b).domains[0].baseline_reset);
    }

    #[test]
    fn disk_replacement_at_same_slot_starts_fresh() {
        let t = Instant::now();
        let mut a = snap(t, vec![dom(5, "vm", 100, 100)]);
        let mut b = snap(t + Duration::from_secs(1), vec![dom(5, "vm", 200, 500)]);
        a.domains[0].vbds[0].backing = Some(Backing {
            path: Some("/old".into()),
            ..Default::default()
        });
        b.domains[0].vbds[0].backing = Some(Backing {
            path: Some("/new".into()),
            ..Default::default()
        });
        assert!(!compute(&a, &b).domains[0].vbds[0].stats_valid);
    }

    /// A VBD on `sr` that did `reqs` reads taking `us` µs in all, plus the
    /// same number of writes taking twice as long.
    fn on_sr(sr: Option<&str>, reqs: u64, us: u64) -> VbdRaw {
        VbdRaw {
            dev: 51712,
            kind: VbdKind::Vbd3,
            oo_reqs: 0,
            rd_reqs: reqs,
            wr_reqs: reqs,
            rd_sects: reqs * 8,
            wr_sects: 0,
            error: false,
            connecting: false,
            ext: Some(VbdExt {
                rd_done: reqs,
                wr_done: reqs,
                rd_usecs: us,
                wr_usecs: us * 2,
                io_errors: 0,
            }),
            backing: sr.map(|s| Backing {
                sr: Some(s.into()),
                vdi: None,
                sr_kind: Some("nfs".into()),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn per_sr_totals() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mk = |id: u32, vbds: Vec<VbdRaw>| DomainRaw {
            vbds,
            ..dom(id, &format!("vm{id}"), 0, 0)
        };
        let zero = |sr| on_sr(sr, 0, 0);
        let prev = snap(
            t0,
            vec![
                mk(
                    1,
                    vec![
                        zero(Some("A")),
                        VbdRaw {
                            dev: 51728,
                            ..zero(Some("B"))
                        },
                    ],
                ),
                mk(2, vec![zero(Some("A"))]),
                mk(3, vec![zero(None)]),
            ],
        );
        let cur = snap(
            t1,
            vec![
                // vm1: 100 reads at 1 ms on A, 10 reads at 5 ms on B.
                mk(
                    1,
                    vec![
                        on_sr(Some("A"), 100, 100_000),
                        VbdRaw {
                            dev: 51728,
                            ..on_sr(Some("B"), 10, 50_000)
                        },
                    ],
                ),
                // vm2: 300 reads at 3 ms on A: the top VM there.
                mk(2, vec![on_sr(Some("A"), 300, 900_000)]),
                // No mapping: not in any SR.
                mk(3, vec![on_sr(None, 1000, 0)]),
            ],
        );
        let r = compute(&prev, &cur);
        assert_eq!(r.srs.len(), 2);
        let a = &r.srs[0];
        assert_eq!((a.sr.as_str(), a.vbds, a.kind.as_deref()), ("A", 2, Some("nfs")));
        // 400 reads + 400 writes over 2 s.
        assert_eq!((a.rd_iops, a.wr_iops), (200.0, 200.0));
        assert_eq!(a.rd_bps, 400.0 * 8.0 * 512.0 / 2.0);
        // Weighted by requests: (100 ms + 900 ms) / 400 = 2.5 ms, not the
        // 2 ms mean of the two VBD averages.
        assert_eq!(a.rd_lat_us, Some(2500.0));
        assert_eq!(a.wr_lat_us, Some(5000.0));
        assert_eq!(
            (a.top_id, a.top_name.as_deref(), a.top_iops),
            (Some(2), Some("vm2"), 300.0)
        );
        let b = &r.srs[1];
        assert_eq!((b.sr.as_str(), b.vbds, b.top_id), ("B", 1, Some(1)));
        assert_eq!(b.rd_lat_us, Some(5000.0));
        assert_eq!(r.domains[0].vbds[1].backing.sr.as_deref(), Some("B"));
    }

    /// SRs plugged into the host get a row even with no active disk on
    /// them (issue #6), and xapi's exact type wins over the local guess.
    #[test]
    fn idle_host_srs_are_listed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let host = |uuid: &str, name: &str, kind: &str| HostSr {
            uuid: uuid.into(),
            name: Some(name.into()),
            kind: Some(kind.into()),
        };
        let mut prev = snap(t0, vec![]);
        let mut cur = snap(t1, vec![]);
        prev.host_srs = vec![host("A", "Local NVMe", "ext")];
        cur.host_srs = vec![host("A", "Local NVMe", "ext"), host("B", "TrueNAS", "nfs")];
        let r = compute(&prev, &cur);
        let rows: Vec<(&str, Option<&str>, usize, f64)> = r
            .srs
            .iter()
            .map(|s| (s.sr.as_str(), s.name.as_deref(), s.vbds, s.iops()))
            .collect();
        assert_eq!(
            rows,
            [("A", Some("Local NVMe"), 0, 0.0), ("B", Some("TrueNAS"), 0, 0.0)]
        );

        // A busy disk on B: B comes first, with xapi's type.
        let mk = |vbd| DomainRaw {
            vbds: vec![vbd],
            ..dom(1, "vm1", 0, 0)
        };
        let mut prev = snap(t0, vec![mk(on_sr(Some("B"), 0, 0))]);
        let mut cur = snap(t1, vec![mk(on_sr(Some("B"), 10, 100))]);
        prev.host_srs = vec![host("A", "Local NVMe", "ext"), host("B", "iSCSI", "lvmoiscsi")];
        cur.host_srs = prev.host_srs.clone();
        let r = compute(&prev, &cur);
        assert_eq!(r.srs[0].sr, "B");
        assert_eq!((r.srs[0].vbds, r.srs[0].kind.as_deref()), (1, Some("lvmoiscsi")));
        assert_eq!((r.srs[1].sr.as_str(), r.srs[1].vbds), ("A", 0));
    }

    /// A domain with `steal` ns of runnable time per vCPU (None: no data).
    fn steal_dom(cpu_ns: u64, steal: &[Option<u64>]) -> DomainRaw {
        DomainRaw {
            vcpus: steal
                .iter()
                .map(|&s| VcpuRaw {
                    online: true,
                    ns: cpu_ns / steal.len() as u64,
                    runnable_ns: s,
                    runstate_at_ns: None,
                })
                .collect(),
            ..dom(7, "vm", cpu_ns, 0)
        }
    }

    #[test]
    fn steal_per_vcpu_and_domain() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let r = compute(
            &snap(t0, vec![steal_dom(0, &[Some(0), Some(1_000)])]),
            // vCPU0 waited 100 ms, vCPU1 300 ms; both ran 500 ms.
            &snap(
                t1,
                vec![steal_dom(1_000_000_000, &[Some(100_000_000), Some(300_001_000)])],
            ),
        );
        let d = &r.domains[0];
        let v: Vec<f64> = d.vcpu_steal_pct.iter().map(|x| x.unwrap()).collect();
        assert!((v[0] - 10.0).abs() < 1e-9 && (v[1] - 30.0).abs() < 1e-9, "{v:?}");
        // Mean over the two vCPUs.
        assert!((d.steal_pct.unwrap() - 20.0).abs() < 1e-9);
        // Host: same definition, over every reporting vCPU.
        assert!((r.host.steal_pct.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn steal_uses_the_hypervisor_sample_time() {
        // Runnable counters 1 s apart in hypervisor time; this process read
        // its clock 2 s apart (descheduled after the second hypercall).
        let at = |d: &mut DomainRaw, t: u64| d.vcpus.iter_mut().for_each(|v| v.runstate_at_ns = Some(t));
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut a = steal_dom(0, &[Some(0)]);
        let mut b = steal_dom(1_000_000_000, &[Some(500_000_000)]);
        at(&mut a, 10_000_000_000);
        at(&mut b, 11_000_000_000);
        let r = compute(&snap(t0, vec![a.clone()]), &snap(t1, vec![b.clone()]));
        assert!((r.domains[0].vcpu_steal_pct[0].unwrap() - 50.0).abs() < 1e-9);
        assert!((r.domains[0].steal_pct.unwrap() - 50.0).abs() < 1e-9);
        assert!((r.host.steal_pct.unwrap() - 50.0).abs() < 1e-9);
        // The other way round (clock read 0.5 s apart): still 50%, not a
        // clamped 100%.
        let r = compute(
            &snap(t0, vec![a.clone()]),
            &snap(t0 + Duration::from_millis(500), vec![b.clone()]),
        );
        assert!((r.domains[0].steal_pct.unwrap() - 50.0).abs() < 1e-9);
        // Without the timestamp (older libxenstat or hypervisor), the wall
        // clock is all there is.
        b.vcpus[0].runstate_at_ns = None;
        let r = compute(&snap(t0, vec![a]), &snap(t1, vec![b]));
        assert!((r.domains[0].steal_pct.unwrap() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn steal_missing_is_none_not_zero() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let r = compute(
            &snap(t0, vec![steal_dom(0, &[None, None])]),
            &snap(t1, vec![steal_dom(1_000, &[None, None])]),
        );
        assert_eq!(r.domains[0].steal_pct, None);
        assert_eq!(r.domains[0].vcpu_steal_pct, vec![None, None]);
        assert_eq!(r.host.steal_pct, None);

        // Data appears only in the newer sample: nothing to diff yet.
        let r = compute(
            &snap(t0, vec![steal_dom(0, &[None])]),
            &snap(t1, vec![steal_dom(1_000, &[Some(5)])]),
        );
        assert_eq!(r.domains[0].steal_pct, None);
    }

    #[test]
    fn steal_domain_wide_fallback() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let mk = |cpu, run| DomainRaw {
            runnable_ns: Some(run),
            ..steal_dom(cpu, &[None, None])
        };
        let r = compute(
            &snap(t0, vec![mk(0, 0)]),
            &snap(t1, vec![mk(600_000_000, 200_000_000)]),
        );
        let d = &r.domains[0];
        // 200 ms over 2 vCPUs x 1 s.
        assert!((d.steal_pct.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(d.vcpu_steal_pct, vec![None, None]);
        assert!((r.host.steal_pct.unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn steal_counter_reset_and_clamp() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // Counter went backwards (domain rebuilt under the same name and
        // id): no negative or huge steal.
        let r = compute(
            &snap(t0, vec![steal_dom(0, &[Some(9_000_000_000)])]),
            &snap(t1, vec![steal_dom(0, &[Some(5)])]),
        );
        assert_eq!(r.domains[0].steal_pct, Some(0.0));
        assert_eq!(r.host.steal_pct, Some(0.0));
        // More runnable time than wall time (clock skew): capped at 100%.
        let r = compute(
            &snap(t0, vec![steal_dom(0, &[Some(0)])]),
            &snap(t1, vec![steal_dom(0, &[Some(3_000_000_000)])]),
        );
        assert_eq!(r.domains[0].vcpu_steal_pct, vec![Some(100.0)]);
        assert_eq!(r.domains[0].steal_pct, Some(100.0));
    }

    #[test]
    fn zero_interval_is_finite() {
        let t0 = Instant::now();
        let r = compute(
            &snap(t0, vec![dom(1, "a", 0, 0)]),
            &snap(t0, vec![dom(1, "a", 1_000, 1)]),
        );
        assert!(r.domains[0].cpu_pct.is_finite());
        assert!(r.host.cpu_busy.is_finite());
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;

    /// Hostile or corrupt counters (e.g. a planted tapdisk stats file) must
    /// not wrap around and hide errors, nor panic in debug builds.
    #[test]
    fn huge_counters_saturate() {
        let now = Instant::now();
        let vbd = VbdRaw {
            dev: 51712,
            kind: VbdKind::Vbd3,
            oo_reqs: u64::MAX,
            rd_reqs: u64::MAX,
            wr_reqs: u64::MAX,
            rd_sects: u64::MAX,
            wr_sects: u64::MAX,
            error: true,
            connecting: false,
            ext: Some(VbdExt {
                rd_done: u64::MAX,
                wr_done: u64::MAX,
                rd_usecs: u64::MAX,
                wr_usecs: u64::MAX,
                io_errors: u64::MAX,
            }),
            backing: None,
        };
        let net = NetRaw {
            id: 0,
            rerrs: u64::MAX,
            terrs: u64::MAX,
            rdrop: u64::MAX,
            tdrop: 1,
            ..Default::default()
        };
        let dom = DomainRaw {
            id: 3,
            name: "x".into(),
            state: DomState::Running,
            flags: flag::RUNNING,
            ssid: 0,
            cpu_ns: u64::MAX,
            vcpus: vec![
                VcpuRaw {
                    online: true,
                    ns: u64::MAX,
                    runnable_ns: Some(u64::MAX),
                    runstate_at_ns: None,
                };
                2
            ],
            cur_mem: u64::MAX,
            max_mem: u64::MAX,
            nets: vec![net.clone(), net],
            vbds: vec![vbd.clone(), vbd],
            vm_uuid: None,
            mem_target: None,
            runnable_ns: Some(u64::MAX),
        };
        let s = Snapshot {
            at: now,
            hostname: String::new(),
            xen_version: String::new(),
            num_cpus: 0,
            cpu_hz: 0,
            tot_mem: 0,
            free_mem: u64::MAX,
            pcpu_idle_ns: Some(vec![(0, u64::MAX)]),
            domains: vec![dom.clone(), DomainRaw { id: 4, ..dom }],
            host_srs: Vec::new(),
        };
        let r = compute(
            &s,
            &Snapshot {
                at: now + std::time::Duration::from_secs(1),
                ..s.clone()
            },
        );
        assert_eq!(r.domains[0].disk_errors, u64::MAX);
        assert_eq!(r.domains[0].net_errs, u64::MAX);
        assert!(r.host.cpu_busy.is_finite());
        assert!(r.host.steal_pct.is_some_and(f64::is_finite));
    }
}
