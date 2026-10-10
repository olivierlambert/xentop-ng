//! Runtime binding to libxenstat.
//!
//! The library is dlopen()ed rather than linked so a single binary runs on
//! any Xen release (libxenstat's soname changes every release) and can pick
//! up a locally patched build through LD_LIBRARY_PATH. Symbols added by the
//! xentop-ng libxenstat patches are optional: when absent, the matching
//! columns simply show "-".

use super::dl;
use super::gaps::{self, HostCollectors};
use super::xapi::Xapi;
use super::xenstore::StorageMap;
use super::{DataStatus, Source, XapiState};
use crate::model::*;
use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use std::ffi::{c_char, c_uint, c_ulonglong, c_void, CStr};
use std::time::Instant;

type P = *mut c_void;

const XENSTAT_ALL: c_uint = 0xf;

struct Api {
    init: unsafe extern "C" fn() -> P,
    uninit: unsafe extern "C" fn(P),
    get_node: unsafe extern "C" fn(P, c_uint) -> P,
    free_node: unsafe extern "C" fn(P),

    node_xen_version: unsafe extern "C" fn(P) -> *const c_char,
    node_tot_mem: unsafe extern "C" fn(P) -> c_ulonglong,
    node_free_mem: unsafe extern "C" fn(P) -> c_ulonglong,
    node_num_domains: unsafe extern "C" fn(P) -> c_uint,
    node_num_cpus: unsafe extern "C" fn(P) -> c_uint,
    node_cpu_hz: unsafe extern "C" fn(P) -> c_ulonglong,
    node_domain_by_index: unsafe extern "C" fn(P, c_uint) -> P,

    domain_id: unsafe extern "C" fn(P) -> c_uint,
    domain_name: unsafe extern "C" fn(P) -> *const c_char,
    domain_cpu_ns: unsafe extern "C" fn(P) -> c_ulonglong,
    domain_num_vcpus: unsafe extern "C" fn(P) -> c_uint,
    domain_vcpu: unsafe extern "C" fn(P, c_uint) -> P,
    domain_cur_mem: unsafe extern "C" fn(P) -> c_ulonglong,
    domain_max_mem: unsafe extern "C" fn(P) -> c_ulonglong,
    domain_dying: unsafe extern "C" fn(P) -> c_uint,
    domain_crashed: unsafe extern "C" fn(P) -> c_uint,
    domain_shutdown: unsafe extern "C" fn(P) -> c_uint,
    domain_paused: unsafe extern "C" fn(P) -> c_uint,
    domain_running: unsafe extern "C" fn(P) -> c_uint,
    // Only for the xentop-compatible output; present in every libxenstat
    // but not needed for anything else, so not required.
    domain_blocked: Option<unsafe extern "C" fn(P) -> c_uint>,
    domain_ssid: Option<unsafe extern "C" fn(P) -> c_uint>,
    domain_num_networks: unsafe extern "C" fn(P) -> c_uint,
    domain_network: unsafe extern "C" fn(P, c_uint) -> P,
    domain_num_vbds: unsafe extern "C" fn(P) -> c_uint,
    domain_vbd: unsafe extern "C" fn(P, c_uint) -> P,

    vcpu_online: unsafe extern "C" fn(P) -> c_uint,
    vcpu_ns: unsafe extern "C" fn(P) -> c_ulonglong,

    network_id: unsafe extern "C" fn(P) -> c_uint,
    network_rbytes: unsafe extern "C" fn(P) -> c_ulonglong,
    network_rpackets: unsafe extern "C" fn(P) -> c_ulonglong,
    network_rerrs: unsafe extern "C" fn(P) -> c_ulonglong,
    network_rdrop: unsafe extern "C" fn(P) -> c_ulonglong,
    network_tbytes: unsafe extern "C" fn(P) -> c_ulonglong,
    network_tpackets: unsafe extern "C" fn(P) -> c_ulonglong,
    network_terrs: unsafe extern "C" fn(P) -> c_ulonglong,
    network_tdrop: unsafe extern "C" fn(P) -> c_ulonglong,

    vbd_type: unsafe extern "C" fn(P) -> c_uint,
    vbd_dev: unsafe extern "C" fn(P) -> c_uint,
    vbd_oo_reqs: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_rd_reqs: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_wr_reqs: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_rd_sects: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_wr_sects: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_error: Option<unsafe extern "C" fn(P) -> bool>,

    ext: Option<ExtApi>,
    pcpu_idle_ns: Option<unsafe extern "C" fn(P, c_uint) -> c_ulonglong>,
    /// Slots in the pCPU idle array (max_cpu_id + 1); may exceed num_cpus.
    num_pcpu_idle: Option<unsafe extern "C" fn(P) -> c_uint>,
    runstate: Option<RunstateApi>,
}

/// Per-vCPU runstate symbols (libxenstat patch 0004; the data also needs
/// the hypervisor side, patch 0003).
struct RunstateApi {
    vcpu_has_runstate: unsafe extern "C" fn(P) -> c_uint,
    vcpu_runnable_ns: unsafe extern "C" fn(P) -> c_ulonglong,
}

/// Symbols from the xentop-ng libxenstat patch.
struct ExtApi {
    vbd_has_ext: unsafe extern "C" fn(P) -> c_uint,
    vbd_rd_reqs_done: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_wr_reqs_done: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_rd_usecs: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_wr_usecs: unsafe extern "C" fn(P) -> c_ulonglong,
    vbd_io_errors: unsafe extern "C" fn(P) -> c_ulonglong,
}

pub struct XenstatSource {
    api: Api,
    handle: P,
    lib_name: String,
    hostname: String,
    /// What libxenstat lacks, read from the host.
    collectors: HostCollectors,
    /// VM UUIDs, balloon targets and VBD -> SR/VDI, from xenstore.
    storage: StorageMap,
    /// SR/VDI/network names, on hosts running xapi.
    xapi: Option<Xapi>,
    status: DataStatus,
    // Keeps the function pointers above valid; declared last so it is
    // dropped after `handle` has been released in Drop.
    _lib: Library,
}

impl XenstatSource {
    pub fn open(explicit: Option<&str>) -> Result<Self> {
        let names = match explicit {
            Some(p) => vec![dl::checked_path(p)?],
            None => dl::versioned("libxenstat"),
        };
        let (lib, lib_name) = dl::open_first(&names).map_err(|e| {
            anyhow!(
                "could not load libxenstat ({}); is this a Xen dom0? Try --demo",
                e.map(|e| e.to_string()).unwrap_or_default()
            )
        })?;

        macro_rules! req {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .with_context(|| format!("{lib_name}: missing symbol {}", $name))?
            };
        }
        macro_rules! opt {
            ($name:literal) => {
                unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .ok()
                    .map(|s| *s)
            };
        }

        let ext = (|| {
            Some(ExtApi {
                vbd_has_ext: opt!("xenstat_vbd_has_ext")?,
                vbd_rd_reqs_done: opt!("xenstat_vbd_rd_reqs_done")?,
                vbd_wr_reqs_done: opt!("xenstat_vbd_wr_reqs_done")?,
                vbd_rd_usecs: opt!("xenstat_vbd_rd_usecs")?,
                vbd_wr_usecs: opt!("xenstat_vbd_wr_usecs")?,
                vbd_io_errors: opt!("xenstat_vbd_io_errors")?,
            })
        })();

        let runstate = (|| {
            Some(RunstateApi {
                vcpu_has_runstate: opt!("xenstat_vcpu_has_runstate")?,
                vcpu_runnable_ns: opt!("xenstat_vcpu_runnable_ns")?,
            })
        })();

        let api = Api {
            init: req!("xenstat_init"),
            uninit: req!("xenstat_uninit"),
            get_node: req!("xenstat_get_node"),
            free_node: req!("xenstat_free_node"),
            node_xen_version: req!("xenstat_node_xen_version"),
            node_tot_mem: req!("xenstat_node_tot_mem"),
            node_free_mem: req!("xenstat_node_free_mem"),
            node_num_domains: req!("xenstat_node_num_domains"),
            node_num_cpus: req!("xenstat_node_num_cpus"),
            node_cpu_hz: req!("xenstat_node_cpu_hz"),
            node_domain_by_index: req!("xenstat_node_domain_by_index"),
            domain_id: req!("xenstat_domain_id"),
            domain_name: req!("xenstat_domain_name"),
            domain_cpu_ns: req!("xenstat_domain_cpu_ns"),
            domain_num_vcpus: req!("xenstat_domain_num_vcpus"),
            domain_vcpu: req!("xenstat_domain_vcpu"),
            domain_cur_mem: req!("xenstat_domain_cur_mem"),
            domain_max_mem: req!("xenstat_domain_max_mem"),
            domain_dying: req!("xenstat_domain_dying"),
            domain_crashed: req!("xenstat_domain_crashed"),
            domain_shutdown: req!("xenstat_domain_shutdown"),
            domain_paused: req!("xenstat_domain_paused"),
            domain_running: req!("xenstat_domain_running"),
            domain_blocked: opt!("xenstat_domain_blocked"),
            domain_ssid: opt!("xenstat_domain_ssid"),
            domain_num_networks: req!("xenstat_domain_num_networks"),
            domain_network: req!("xenstat_domain_network"),
            domain_num_vbds: req!("xenstat_domain_num_vbds"),
            domain_vbd: req!("xenstat_domain_vbd"),
            vcpu_online: req!("xenstat_vcpu_online"),
            vcpu_ns: req!("xenstat_vcpu_ns"),
            network_id: req!("xenstat_network_id"),
            network_rbytes: req!("xenstat_network_rbytes"),
            network_rpackets: req!("xenstat_network_rpackets"),
            network_rerrs: req!("xenstat_network_rerrs"),
            network_rdrop: req!("xenstat_network_rdrop"),
            network_tbytes: req!("xenstat_network_tbytes"),
            network_tpackets: req!("xenstat_network_tpackets"),
            network_terrs: req!("xenstat_network_terrs"),
            network_tdrop: req!("xenstat_network_tdrop"),
            vbd_type: req!("xenstat_vbd_type"),
            vbd_dev: req!("xenstat_vbd_dev"),
            vbd_oo_reqs: req!("xenstat_vbd_oo_reqs"),
            vbd_rd_reqs: req!("xenstat_vbd_rd_reqs"),
            vbd_wr_reqs: req!("xenstat_vbd_wr_reqs"),
            vbd_rd_sects: req!("xenstat_vbd_rd_sects"),
            vbd_wr_sects: req!("xenstat_vbd_wr_sects"),
            vbd_error: opt!("xenstat_vbd_error"),
            ext,
            pcpu_idle_ns: opt!("xenstat_node_pcpu_idle_ns"),
            num_pcpu_idle: opt!("xenstat_node_num_pcpu_idle"),
            runstate,
        };

        let handle = unsafe { (api.init)() };
        if handle.is_null() {
            bail!("xenstat_init() failed: are you root in dom0 (need /dev/xen/privcmd and xenstore)?");
        }
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "xen".into());

        let collectors = HostCollectors::new(api.pcpu_idle_ns.is_none());
        Ok(Self {
            api,
            handle,
            lib_name,
            hostname,
            collectors,
            storage: StorageMap::open(),
            xapi: None,
            status: DataStatus::default(),
            _lib: lib,
        })
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    crate::fmt::sanitize(&unsafe { CStr::from_ptr(p) }.to_string_lossy())
}

impl XenstatSource {
    /// Copy one libxenstat node out into a [`Snapshot`], as the library
    /// reports it: the only place its data structures are touched.
    fn read_node(&self) -> Result<Snapshot> {
        let a = &self.api;
        // Stamp before the call: the domain counters are read near its start,
        // and the call itself can take most of a second while dom0 builds or
        // tears down a domain. A stamp taken after it would divide a counter
        // delta by an interval that does not match it.
        let at = Instant::now();
        let node = unsafe { (a.get_node)(self.handle, XENSTAT_ALL) };
        if node.is_null() {
            bail!("xenstat_get_node() failed");
        }

        // SAFETY: every pointer below is owned by `node` and stays valid
        // until xenstat_free_node(), which we call once we've copied out.
        let snap = unsafe {
            let num_cpus = (a.node_num_cpus)(node);
            let slots = a.num_pcpu_idle.map(|f| f(node)).unwrap_or(num_cpus);
            let pcpu_idle_ns = a
                .pcpu_idle_ns
                // Offline CPUs (and holes left by smt=0) report 0.
                .map(|f| {
                    (0..slots)
                        .map(|c| (c, f(node, c)))
                        .filter(|&(_, ns)| ns != 0)
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());

            let n = (a.node_num_domains)(node);
            let mut domains = Vec::with_capacity(n as usize);
            for i in 0..n {
                let dp = (a.node_domain_by_index)(node, i);
                if dp.is_null() {
                    continue;
                }
                let bit = |f: unsafe extern "C" fn(P) -> c_uint, b: u8| if f(dp) != 0 { b } else { 0 };
                let flags = bit(a.domain_dying, flag::DYING)
                    | bit(a.domain_shutdown, flag::SHUTDOWN)
                    | a.domain_blocked.map_or(0, |f| bit(f, flag::BLOCKED))
                    | bit(a.domain_crashed, flag::CRASHED)
                    | bit(a.domain_paused, flag::PAUSED)
                    | bit(a.domain_running, flag::RUNNING);
                let state = if (a.domain_crashed)(dp) != 0 {
                    DomState::Crashed
                } else if (a.domain_dying)(dp) != 0 {
                    DomState::Dying
                } else if (a.domain_shutdown)(dp) != 0 {
                    DomState::Shutdown
                } else if (a.domain_paused)(dp) != 0 {
                    DomState::Paused
                } else if (a.domain_running)(dp) != 0 {
                    DomState::Running
                } else {
                    DomState::Blocked
                };

                let vcpus = (0..(a.domain_num_vcpus)(dp))
                    .filter_map(|j| {
                        let v = (a.domain_vcpu)(dp, j);
                        (!v.is_null()).then(|| VcpuRaw {
                            online: (a.vcpu_online)(v) != 0,
                            ns: (a.vcpu_ns)(v),
                            runnable_ns: a
                                .runstate
                                .as_ref()
                                .filter(|r| (r.vcpu_has_runstate)(v) != 0)
                                .map(|r| (r.vcpu_runnable_ns)(v)),
                        })
                    })
                    .collect();

                let nets = (0..(a.domain_num_networks)(dp))
                    .filter_map(|j| {
                        let x = (a.domain_network)(dp, j);
                        (!x.is_null()).then(|| NetRaw {
                            id: (a.network_id)(x),
                            network: None,
                            rbytes: (a.network_rbytes)(x),
                            rpackets: (a.network_rpackets)(x),
                            rerrs: (a.network_rerrs)(x),
                            rdrop: (a.network_rdrop)(x),
                            tbytes: (a.network_tbytes)(x),
                            tpackets: (a.network_tpackets)(x),
                            terrs: (a.network_terrs)(x),
                            tdrop: (a.network_tdrop)(x),
                        })
                    })
                    .collect();

                let vbds = (0..(a.domain_num_vbds)(dp))
                    .filter_map(|j| {
                        let x = (a.domain_vbd)(dp, j);
                        if x.is_null() {
                            return None;
                        }
                        let ext = a.ext.as_ref().and_then(|e| {
                            ((e.vbd_has_ext)(x) != 0).then(|| VbdExt {
                                rd_done: (e.vbd_rd_reqs_done)(x),
                                wr_done: (e.vbd_wr_reqs_done)(x),
                                rd_usecs: (e.vbd_rd_usecs)(x),
                                wr_usecs: (e.vbd_wr_usecs)(x),
                                io_errors: (e.vbd_io_errors)(x),
                            })
                        });
                        Some(VbdRaw {
                            dev: (a.vbd_dev)(x),
                            kind: VbdKind::from_xenstat((a.vbd_type)(x)),
                            oo_reqs: (a.vbd_oo_reqs)(x),
                            rd_reqs: (a.vbd_rd_reqs)(x),
                            wr_reqs: (a.vbd_wr_reqs)(x),
                            rd_sects: (a.vbd_rd_sects)(x),
                            wr_sects: (a.vbd_wr_sects)(x),
                            error: a.vbd_error.map(|f| f(x)).unwrap_or(false),
                            connecting: false,
                            ext,
                            backing: None,
                        })
                    })
                    .collect();

                domains.push(DomainRaw {
                    id: (a.domain_id)(dp),
                    name: cstr((a.domain_name)(dp)),
                    state,
                    flags,
                    ssid: a.domain_ssid.map(|f| f(dp)).unwrap_or(0),
                    cpu_ns: (a.domain_cpu_ns)(dp),
                    vcpus,
                    cur_mem: (a.domain_cur_mem)(dp),
                    max_mem: (a.domain_max_mem)(dp),
                    nets,
                    vbds,
                    vm_uuid: None,
                    mem_target: None,
                    runnable_ns: None,
                });
            }

            Snapshot {
                at,
                hostname: self.hostname.clone(),
                xen_version: cstr((a.node_xen_version)(node)),
                num_cpus,
                cpu_hz: (a.node_cpu_hz)(node),
                tot_mem: (a.node_tot_mem)(node),
                free_mem: (a.node_free_mem)(node),
                pcpu_idle_ns,
                domains,
                host_srs: Vec::new(),
            }
        };
        unsafe { (a.free_node)(node) };
        Ok(snap)
    }
}

impl Source for XenstatSource {
    fn sample(&mut self) -> Result<Snapshot> {
        let mut snap = self.read_node()?;
        // Storage first: it tells disks still connecting from failed reads,
        // which latency coverage below must not count as missing.
        self.status.storage = self.storage.fill(&mut snap);
        let g = gaps::fill(&mut snap, self.api.ext.is_some(), &mut self.collectors);
        self.status.pcpu = g.pcpu;
        self.status.vifs = g.vifs;
        self.status.steal = g.steal;
        self.status.vbd_latency = g.vbd_latency;
        self.status.vbd_latency_coverage = gaps::latency_coverage(&snap);
        self.status.steal_coverage = gaps::steal_coverage(&snap);
        if let Some(x) = &mut self.xapi {
            x.fill(&mut snap);
            self.status.xapi = x.state();
        }
        Ok(snap)
    }

    fn describe(&self) -> String {
        self.lib_name.clone()
    }

    fn status(&self) -> DataStatus {
        self.status.clone()
    }
}

impl XenstatSource {
    /// Name SRs, VDIs and networks through xapi when this host runs it.
    /// Off (`--no-xapi`), or on plain Xen, only UUIDs are shown.
    pub fn with_xapi(mut self, on: bool) -> Self {
        self.xapi = if on { Xapi::open() } else { None };
        self.status.xapi = match (on, &self.xapi) {
            (false, _) => XapiState::Disabled,
            (true, None) => XapiState::Absent,
            (true, Some(x)) => x.state(),
        };
        self
    }
}

impl Drop for XenstatSource {
    fn drop(&mut self) {
        unsafe { (self.api.uninit)(self.handle) };
    }
}
