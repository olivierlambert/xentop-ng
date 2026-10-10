//! Storage and VM identity from xenstore: which SR/VDI (or file/device)
//! backs each VBD, each domain's VM UUID, and its balloon target.
//!
//! libxenstore is dlopen()ed like libxenstat; only its long-stable core
//! (xs_open, xs_read, xs_close) is used. Per sample this costs two reads per
//! domain (`vm`, `memory/target`) and one per VBD (`params`); the rest is cached
//! and only re-read when `params` changes or a domain appears.
//!
//! xenstore paths used (backend nodes are written by the dom0 toolstack,
//! but values are still treated as untrusted: bounded, sanitised, and
//! UUIDs validated before they are used in any path):
//!
//! - `/local/domain/<id>/vm` = "/vm/<vm uuid>"
//! - `/local/domain/<id>/memory/target` = balloon target in KiB
//! - `/local/domain/0/backend/<vbd3|vbd|qdisk|tap>/<id>/<dev>/params`:
//!   XCP-ng: "/dev/sm/backend/<sr>/<vdi>"; plain Xen: a device or file
//!   path, possibly with a "<format>:" prefix
//! - `.../sm-data/vdi-uuid` and `.../sm-data/mem-pool` (the SR), only when
//!   `params` doesn't say, whatever the backend kind
//!
//! The SR flavour (ext, nfs, lvm...) is not in xenstore. It is worked out
//! once per SR from where `/dev/sm/phy/<sr>/<vdi>` points and, for file
//! SRs, the file system type of `/run/sr-mount/<sr>` in /proc/mounts.

use super::Avail;
use crate::model::{Backing, Snapshot, VbdKind};
use libloading::Library;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_uint, c_ulong, c_void, CString};
use std::io::Read;

/// Longest xenstore value we copy out (xenstored's own limit is 4096).
const MAX_VALUE: usize = 4096;
/// Longest backing path we keep.
const MAX_PATH: usize = 512;
/// Bounds on xenstore round trips per sample.
const MAX_DOMS: usize = 4096;
const MAX_VBDS: usize = 16384;
/// Samples between attempts to identify an SR whose type we couldn't tell.
const KIND_RETRY: u64 = 30;

type Handle = *mut c_void;

/// Minimal libxenstore binding.
struct Xs {
    h: Handle,
    read: unsafe extern "C" fn(Handle, u32, *const c_char, *mut c_uint) -> *mut c_void,
    close: unsafe extern "C" fn(Handle),
    _lib: Library,
}

impl Xs {
    fn open() -> Option<Self> {
        let (lib, _) =
            super::dl::open_first(&["libxenstore.so.4", "libxenstore.so.3.0", "libxenstore.so"]).ok()?;
        unsafe {
            type Open = unsafe extern "C" fn(c_ulong) -> Handle;
            let open: Open = *lib.get(b"xs_open\0").ok()?;
            let read = *lib.get(b"xs_read\0").ok()?;
            let close = *lib.get(b"xs_close\0").ok()?;
            // XS_OPEN_READONLY states our intent; newer libraries ignore it,
            // older ones need the read-only socket, so fall back to 0.
            let mut h = open(1);
            if h.is_null() {
                h = open(0);
            }
            if h.is_null() {
                return None;
            }
            Some(Xs {
                h,
                read,
                close,
                _lib: lib,
            })
        }
    }

    /// Read a node outside any transaction. Values are cut at MAX_VALUE
    /// bytes and at the first NUL.
    fn read(&self, path: &str) -> Option<String> {
        let c = CString::new(path).ok()?;
        let mut len: c_uint = 0;
        // SAFETY: valid handle and NUL-terminated path; xs_read returns a
        // malloc()ed buffer of `len` bytes (or NULL) that we must free().
        let p = unsafe { (self.read)(self.h, 0, c.as_ptr(), &mut len) };
        if p.is_null() {
            return None;
        }
        let n = (len as usize).min(MAX_VALUE);
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, n) };
        let bytes = bytes.split(|&b| b == 0).next().unwrap_or(&[]);
        let s = String::from_utf8_lossy(bytes).into_owned();
        unsafe { libc::free(p) };
        Some(s)
    }
}

impl Drop for Xs {
    fn drop(&mut self) {
        unsafe { (self.close)(self.h) };
    }
}

/// Fills in VM UUIDs, balloon targets and VBD backings on each sample.
pub struct StorageMap {
    xs: Option<Xs>,
    /// (domid, dev) -> (params, backing): re-parsed only when params change.
    vbds: HashMap<(u32, u32), (String, Option<Backing>)>,
    kinds: SrKinds,
    identities: VmIdentities,
}

/// Retain only successful UUID reads while the observed domain remains
/// continuous. Always retry xenstore, including after a failed first read.
#[derive(Default)]
struct VmIdentities(HashMap<u32, (String, u64, String)>);

impl VmIdentities {
    fn resolve(&mut self, id: u32, name: &str, cpu_ns: u64, observed: Option<String>) -> Option<String> {
        let uuid = observed.or_else(|| {
            self.0
                .get(&id)
                .filter(|(n, cpu, _)| n == name && cpu_ns >= *cpu)
                .map(|(_, _, uuid)| uuid.clone())
        });
        if let Some(uuid) = &uuid {
            self.0.insert(id, (name.into(), cpu_ns, uuid.clone()));
        } else {
            self.0.remove(&id);
        }
        uuid
    }
    fn retain(&mut self, snap: &Snapshot) {
        let present: HashSet<_> = snap.domains.iter().map(|d| d.id).collect();
        self.0.retain(|id, _| present.contains(id));
    }
}

/// SR UUID -> flavour, worked out once per SR.
#[derive(Default)]
struct SrKinds {
    known: HashMap<String, String>,
    /// SR UUID -> sample of the last failed attempt.
    tried: HashMap<String, u64>,
    tick: u64,
    /// /proc/mounts, read at most once per sample and only when needed.
    mounts: Option<String>,
}

impl StorageMap {
    pub fn open() -> Self {
        StorageMap {
            xs: Xs::open(),
            vbds: HashMap::new(),
            kinds: SrKinds::default(),
            identities: VmIdentities::default(),
        }
    }

    /// Annotate `snap` and say how complete the mapping is.
    pub fn fill(&mut self, snap: &mut Snapshot) -> Avail {
        self.identities.retain(snap);
        self.kinds.tick += 1;
        self.kinds.mounts = None;
        let Some(xs) = &self.xs else {
            return Avail::Missing;
        };
        let (mut total, mut mapped) = (0usize, 0usize);
        let mut reads = 0usize;

        // Forget domains and disks that are gone.
        let disks: HashSet<(u32, u32)> = snap
            .domains
            .iter()
            .flat_map(|d| d.vbds.iter().map(|v| (d.id, v.dev)))
            .collect();
        self.vbds.retain(|k, _| disks.contains(k));

        for d in snap.domains.iter_mut().take(MAX_DOMS) {
            let base = format!("/local/domain/{}", d.id);
            // Read identity each time: domids are reusable, names are mutable,
            // and a failed read at startup must not be cached forever.
            let observed = xs.read(&format!("{base}/vm")).and_then(|p| vm_uuid(&p));
            d.vm_uuid = self.identities.resolve(d.id, &d.name, d.cpu_ns, observed);
            d.mem_target = xs
                .read(&format!("{base}/memory/target"))
                .and_then(|s| parse_kib(&s));

            for v in &mut d.vbds {
                let Some(dir) = backend_dir(v.kind) else {
                    continue;
                };
                total += 1;
                reads += 1;
                if reads > MAX_VBDS {
                    break;
                }
                let node = format!("/local/domain/0/backend/{dir}/{}/{}", d.id, v.dev);
                if v.error && backend_connecting(xs.read(&format!("{node}/state")).as_deref()) {
                    // tapdisk creates its stats file only once the guest's
                    // frontend connects (seconds into a boot): nothing to
                    // read yet is not a failed read.
                    v.error = false;
                    v.connecting = true;
                }
                let key = (d.id, v.dev);
                let backing = match xs.read(&format!("{node}/params")) {
                    // Transient failure, or the backend going away during an
                    // unplug. Either way it is still the same device: keep
                    // its last known backing so the failure doesn't look like
                    // a disk replacement and reset its baseline. Departed
                    // disks are forgotten above.
                    None => self.vbds.get(&key).and_then(|(_, b)| b.clone()),
                    Some(params) => match self.vbds.get(&key) {
                        Some((p, b)) if *p == params => b.clone(),
                        _ => {
                            let mut b = parse_params(&params);
                            if b.sr.is_none() && b.vdi.is_none() {
                                // Not a path we understand: SM also publishes
                                // the VDI and its SR ("mem-pool") under sm-data.
                                // Not only for vbd3: an SM driver that attaches
                                // a block device for blkback (params "/dev/loop0")
                                // publishes the same keys under backend/vbd.
                                let sm =
                                    |k: &str| xs.read(&format!("{node}/sm-data/{k}")).and_then(|s| uuid(&s));
                                b.vdi = sm("vdi-uuid");
                                if b.vdi.is_some() {
                                    b.sr = sm("mem-pool");
                                }
                            }
                            let b = (b != Backing::default()).then_some(b);
                            self.vbds.insert(key, (params, b.clone()));
                            b
                        }
                    },
                };
                v.backing = backing.map(|mut b| {
                    if b.sr_kind.is_none() {
                        if let Some(sr) = b.sr.clone() {
                            b.sr_kind = self.kinds.get(&sr, b.vdi.as_deref());
                        }
                    }
                    b
                });
                mapped += v.backing.is_some() as usize;
            }
        }
        match (total, mapped) {
            (0, _) => Avail::NotApplicable,
            (_, 0) => Avail::Missing,
            _ if mapped < total => Avail::Partial,
            _ => Avail::Fallback,
        }
    }
}

impl SrKinds {
    /// The SR's flavour, worked out from where SM's per-VDI link points.
    fn get(&mut self, sr: &str, vdi: Option<&str>) -> Option<String> {
        if let Some(k) = self.known.get(sr) {
            return Some(k.clone());
        }
        if self.tried.get(sr).is_some_and(|&t| self.tick < t + KIND_RETRY) {
            return None;
        }
        self.tried.insert(sr.to_string(), self.tick);
        // Both UUIDs are validated, so this path can't escape /dev/sm.
        let target = std::fs::read_link(format!("/dev/sm/phy/{sr}/{}", vdi?)).ok()?;
        let target = target.to_str()?;
        let mounts = self.mounts.get_or_insert_with(read_mounts);
        let k = kind_from_target(target, sr, mounts)?;
        self.tried.remove(sr);
        if self.known.len() < MAX_DOMS {
            self.known.insert(sr.to_string(), k.clone());
        }
        Some(k)
    }
}

fn read_mounts() -> String {
    let mut s = String::new();
    if let Ok(f) = std::fs::File::open("/proc/self/mounts") {
        let _ = f.take(1 << 20).read_to_string(&mut s);
    }
    s
}

/// xenstore backend directory for each libxenstat VBD type.
fn backend_dir(k: VbdKind) -> Option<&'static str> {
    Some(match k {
        VbdKind::Vbd3 => "vbd3",
        VbdKind::Blkback => "vbd",
        VbdKind::Qdisk => "qdisk",
        VbdKind::Tap => "tap",
        VbdKind::Unknown => return None,
    })
}

/// A canonical, lower-case UUID, or None.
pub fn uuid(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    let ok = b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then(|| s.to_ascii_lowercase())
}

/// "/vm/<uuid>" -> uuid.
fn vm_uuid(path: &str) -> Option<String> {
    uuid(path.trim().strip_prefix("/vm/")?)
}

/// Decimal KiB -> bytes.
fn parse_kib(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()?.checked_mul(1024)
}

/// Split a device-mapper name into its VG and LV: '-' separates them and
/// "--" stands for a literal '-'.
fn split_dm(name: &str) -> Option<(String, String)> {
    let mut parts = vec![String::new()];
    let mut it = name.chars().peekable();
    while let Some(c) = it.next() {
        if c == '-' {
            if it.peek() == Some(&'-') {
                it.next();
                parts.last_mut()?.push('-');
            } else {
                parts.push(String::new());
            }
        } else {
            parts.last_mut()?.push(c);
        }
    }
    (parts.len() == 2).then(|| {
        let lv = parts.pop().unwrap_or_default();
        (parts.pop().unwrap_or_default(), lv)
    })
}

/// LVM-based SRs: VG "VG_XenStorage-<sr>", LV "VHD-<vdi>" (also "LV-",
/// "QCOW2-").
fn lvm_names(vg: &str, lv: &str) -> Option<Backing> {
    let sr = uuid(vg.strip_prefix("VG_XenStorage-")?)?;
    let vdi = ["VHD-", "LV-", "QCOW2-", "RAW-"]
        .iter()
        .find_map(|p| lv.strip_prefix(p))
        .and_then(uuid);
    Some(Backing {
        sr: Some(sr),
        vdi,
        sr_kind: Some("lvm".into()),
        ..Default::default()
    })
}

/// Work out SR/VDI, or a plain path, from a backend `params` value (or any
/// path SM or tapdisk uses for a VDI).
pub fn parse_params(raw: &str) -> Backing {
    let mut p = raw.trim();
    // "aio:/path", "vhd:/path", "qcow2:/path": drop the format prefix.
    if let Some((fmt, rest)) = p.split_once(':') {
        if rest.starts_with('/')
            && !fmt.is_empty()
            && fmt.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            p = rest;
        }
    }
    if p.is_empty() {
        return Backing::default();
    }
    let norm = p.strip_prefix("/var/run/").map(|r| format!("/run/{r}"));
    let p_norm = norm.as_deref().unwrap_or(p);
    let seg: Vec<&str> = p_norm.split('/').collect();

    // ["", "dev", "sm", "backend"|"phy", sr, vdi]
    if let ["", "dev", "sm", "backend" | "phy", sr, vdi] = seg[..] {
        if let (Some(sr), Some(vdi)) = (uuid(sr), uuid(vdi)) {
            return Backing {
                sr: Some(sr),
                vdi: Some(vdi),
                ..Default::default()
            };
        }
    }
    // File SRs (ext, nfs, smb, ...): /run/sr-mount/<sr>/<vdi>.vhd
    if let ["", "run", "sr-mount", sr, file] = seg[..] {
        if let Some(sr) = uuid(sr) {
            let vdi = file
                .rsplit_once('.')
                .and_then(|(stem, _)| uuid(stem))
                .or_else(|| uuid(file));
            let path = vdi.is_none().then(|| clean_path(p));
            return Backing {
                sr: Some(sr),
                vdi,
                path,
                ..Default::default()
            };
        }
    }
    // LVM SRs: /dev/VG_XenStorage-<sr>/VHD-<vdi>, or the device-mapper name.
    if let ["", "dev", vg, lv] = seg[..] {
        if let Some(b) = lvm_names(vg, lv) {
            return b;
        }
    }
    if let ["", "dev", "mapper", dm] = seg[..] {
        if let Some(b) = split_dm(dm).and_then(|(vg, lv)| lvm_names(&vg, &lv)) {
            return b;
        }
    }
    Backing {
        path: Some(clean_path(p)),
        ..Default::default()
    }
}

/// Sanitised, bounded copy of a path for display.
fn clean_path(p: &str) -> String {
    let s = crate::fmt::sanitize(p);
    if s.chars().count() <= MAX_PATH {
        return s;
    }
    // Keep the tail: that's the part that tells disks apart.
    let skip = s.chars().count() - MAX_PATH;
    s.chars().skip(skip).collect()
}

/// File system type mounted at `/run/sr-mount/<sr>`, from /proc/mounts.
fn mount_fstype(mounts: &str, sr: &str) -> Option<String> {
    let want = [format!("/run/sr-mount/{sr}"), format!("/var/run/sr-mount/{sr}")];
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (_, mnt, fs) = (f.next()?, f.next()?, f.next()?);
        want.iter().any(|w| w == mnt).then(|| fs.to_string())
    })
}

/// Short SR flavour from a file system type.
fn kind_from_fstype(fs: &str) -> String {
    match fs {
        "ext4" | "ext3" | "ext2" => "ext".into(),
        f if f.starts_with("nfs") => "nfs".into(),
        "cifs" | "smb3" => "smb".into(),
        "gfs2" | "xfs" | "zfs" | "glusterfs" | "ceph" | "ocfs2" => fs.into(),
        f => crate::fmt::sanitize(&f.chars().take(8).collect::<String>()),
    }
}

/// SR flavour from where `/dev/sm/phy/<sr>/<vdi>` points.
fn kind_from_target(target: &str, sr: &str, mounts: &str) -> Option<String> {
    let b = parse_params(target);
    if b.sr_kind.is_some() {
        return b.sr_kind;
    }
    if b.sr.as_deref() == Some(sr) {
        // A file under /run/sr-mount/<sr>; not a mount point of its own
        // (e.g. a local ISO directory) means a plain directory.
        return Some(
            mount_fstype(mounts, sr)
                .map(|f| kind_from_fstype(&f))
                .unwrap_or("file".into()),
        );
    }
    None
}

/// A backend not yet in XenbusStateConnected (4). Unreadable or unknown
/// state is not evidence of connecting: the error stands.
fn backend_connecting(state: Option<&str>) -> bool {
    state
        .and_then(|s| s.trim().parse::<u8>().ok())
        .is_some_and(|s| s < 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: &str = "ac70e429-0dec-3ccd-1d24-2713c6104b65";
    const VDI: &str = "86e4e84e-f30b-4a86-b634-1c38f02e1685";

    fn sm(sr: &str, vdi: &str) -> Backing {
        Backing {
            sr: Some(sr.into()),
            vdi: Some(vdi.into()),
            ..Default::default()
        }
    }

    #[test]
    fn backend_state_before_connected_is_connecting() {
        for s in ["1", "2", "3", " 3\n"] {
            assert!(backend_connecting(Some(s)), "{s:?}");
        }
        // Connected, closing/closed, unknown or unreadable: keep the error.
        for s in [Some("4"), Some("5"), Some("6"), Some("x"), Some(""), None] {
            assert!(!backend_connecting(s), "{s:?}");
        }
    }

    #[test]
    fn cached_uuid_preserves_rates_until_a_different_uuid_is_observed() {
        use crate::source::{
            demo::{DemoConfig, DemoSource},
            Source,
        };
        let mut ids = VmIdentities::default();
        let mut a = DemoSource::new(&DemoConfig::default()).sample().unwrap();
        a.domains.truncate(1);
        let d = &mut a.domains[0];
        d.vm_uuid = ids.resolve(d.id, &d.name, d.cpu_ns, Some("uuid-a".into()));
        let mut b = a.clone();
        b.at += std::time::Duration::from_secs(1);
        let d = &mut b.domains[0];
        d.cpu_ns += 100;
        d.vm_uuid = ids.resolve(d.id, &d.name, d.cpu_ns, None);
        assert!(!crate::model::compute(&a, &b).domains[0].baseline_reset);
        let mut c = b.clone();
        c.at += std::time::Duration::from_secs(1);
        let d = &mut c.domains[0];
        d.cpu_ns += 100;
        d.vm_uuid = ids.resolve(d.id, &d.name, d.cpu_ns, Some("uuid-b".into()));
        assert!(crate::model::compute(&b, &c).domains[0].baseline_reset);
        c.domains.clear();
        ids.retain(&c);
        assert!(ids.0.is_empty(), "departed domains cannot leave cached UUIDs");
    }

    #[test]
    fn transient_uuid_failures_keep_identity_but_replacements_do_not() {
        let mut ids = VmIdentities::default();
        let a = Some("uuid-a".to_string());
        let b = Some("uuid-b".to_string());
        assert_eq!(ids.resolve(5, "vm", 10, None), None);
        assert_eq!(ids.resolve(5, "vm", 20, a.clone()), a);
        assert_eq!(ids.resolve(5, "vm", 30, None), a);
        assert_eq!(ids.resolve(5, "vm", 40, None), a);
        assert_eq!(ids.resolve(5, "vm", 50, b.clone()), b);
        assert_eq!(ids.resolve(5, "rename", 60, b.clone()), b);
        assert_eq!(ids.resolve(5, "rename", 70, None), b);
        assert_eq!(
            ids.resolve(5, "rename", 1, None),
            None,
            "counter rollback invalidates cached identity"
        );
        ids.resolve(5, "rename", 10, a);
        assert_eq!(
            ids.resolve(5, "other", 20, None),
            None,
            "unverified rename is conservative"
        );
    }

    #[test]
    fn uuids() {
        assert_eq!(uuid(SR).as_deref(), Some(SR));
        assert_eq!(
            uuid(" AC70E429-0DEC-3CCD-1D24-2713C6104B65 ").as_deref(),
            Some(SR)
        );
        for bad in [
            "",
            "ac70e429",
            "ac70e429-0dec-3ccd-1d24-2713c6104b6",
            "ac70e429-0dec-3ccd-1d24-2713c6104b65x",
            "ac70e429x0dec-3ccd-1d24-2713c6104b65",
            "../../../../../etc/passwd/aaaaaaaaaaa",
            "ac70e429-0dec-3ccd-1d24-2713c6104bg5",
        ] {
            assert_eq!(uuid(bad), None, "{bad}");
        }
        assert_eq!(
            vm_uuid("/vm/0eab0d7e-cdae-1386-91f5-2a06455f82f4").as_deref(),
            Some("0eab0d7e-cdae-1386-91f5-2a06455f82f4")
        );
        assert_eq!(vm_uuid("/vm/0eab0d7e-cdae-1386-91f5-2a06455f82f4/x"), None);
        assert_eq!(vm_uuid("0eab0d7e-cdae-1386-91f5-2a06455f82f4"), None);
    }

    #[test]
    fn kib() {
        assert_eq!(parse_kib("8388608"), Some(8 << 30));
        assert_eq!(parse_kib(" 4194304\n"), Some(4 << 30));
        for bad in ["", "-1", "1e9", "12 34", "99999999999999999999999"] {
            assert_eq!(parse_kib(bad), None, "{bad}");
        }
        // Would overflow once turned into bytes.
        assert_eq!(parse_kib("18446744073709551615"), None);
    }

    #[test]
    fn sm_paths() {
        // XCP-ng 8.3 vbd3 params.
        assert_eq!(parse_params(&format!("/dev/sm/backend/{SR}/{VDI}")), sm(SR, VDI));
        assert_eq!(parse_params(&format!("/dev/sm/phy/{SR}/{VDI}")), sm(SR, VDI));
        // Empty CD drive.
        assert_eq!(parse_params(""), Backing::default());
    }

    #[test]
    fn file_sr() {
        let want = sm(SR, VDI);
        for p in [
            format!("/var/run/sr-mount/{SR}/{VDI}.vhd"),
            format!("/run/sr-mount/{SR}/{VDI}.qcow2"),
            // tap-ctl's view: "<driver>:<path>".
            format!("vhd:/var/run/sr-mount/{SR}/{VDI}.vhd"),
        ] {
            assert_eq!(parse_params(&p), want, "{p}");
        }
        // An ISO in an ISO SR: an SR but no VDI UUID in the name.
        let iso = parse_params(&format!("/var/run/sr-mount/{SR}/debian-12.iso"));
        assert_eq!(iso.sr.as_deref(), Some(SR));
        assert_eq!(iso.vdi, None);
        assert_eq!(iso.path, Some(format!("/var/run/sr-mount/{SR}/debian-12.iso")));
    }

    #[test]
    fn lvm_sr() {
        let want = Backing {
            sr_kind: Some("lvm".into()),
            ..sm(SR, VDI)
        };
        assert_eq!(parse_params(&format!("/dev/VG_XenStorage-{SR}/VHD-{VDI}")), want);
        assert_eq!(parse_params(&format!("/dev/VG_XenStorage-{SR}/LV-{VDI}")), want);
        let dm = format!(
            "/dev/mapper/VG_XenStorage--{}-VHD--{}",
            SR.replace('-', "--"),
            VDI.replace('-', "--")
        );
        assert_eq!(parse_params(&dm), want);
        // A VG of the SR but an LV that isn't a VDI (e.g. MGT).
        let mgt = parse_params(&format!("/dev/VG_XenStorage-{SR}/MGT"));
        assert_eq!((mgt.sr.as_deref(), mgt.vdi), (Some(SR), None));
    }

    #[test]
    fn plain_xen_paths() {
        let b = parse_params("/dev/vg0/web01-disk");
        assert_eq!((&b.sr, &b.vdi), (&None, &None));
        assert_eq!(b.path.as_deref(), Some("/dev/vg0/web01-disk"));
        assert_eq!(b.group().as_deref(), Some("/dev/vg0"));
        let q = parse_params("aio:/var/lib/xen/images/db.qcow2");
        assert_eq!(q.path.as_deref(), Some("/var/lib/xen/images/db.qcow2"));
        assert_eq!(q.group().as_deref(), Some("/var/lib/xen/images"));
        assert_eq!(parse_params("/disk.img").group().as_deref(), Some("/"));
        // Not a format prefix.
        assert_eq!(parse_params("a b:/x").path.as_deref(), Some("a b:/x"));
        // Hostile values come out printable and bounded.
        let evil = format!("/x/\x1b]0;pwn\x07{}", "a".repeat(10_000));
        let e = parse_params(&evil).path.unwrap();
        assert!(e.chars().count() <= MAX_PATH && !e.contains('\x1b'));
        // A SR-looking path with a bad UUID is just a path.
        let bad = parse_params("/dev/sm/backend/../../etc/passwd");
        assert_eq!((bad.sr, bad.vdi), (None, None));
    }

    #[test]
    fn dm_names() {
        assert_eq!(split_dm("vg--a-lv--b"), Some(("vg-a".into(), "lv-b".into())));
        assert_eq!(split_dm("vg-lv"), Some(("vg".into(), "lv".into())));
        assert_eq!(split_dm("vg"), None);
        assert_eq!(split_dm("a-b-c"), None);
    }

    #[test]
    fn sr_kinds() {
        let mounts = include_str!("testdata/xcpng83-mounts.txt");
        assert_eq!(mount_fstype(mounts, SR).as_deref(), Some("ext4"));
        let t = format!("/var/run/sr-mount/{SR}/{VDI}.vhd");
        assert_eq!(kind_from_target(&t, SR, mounts).as_deref(), Some("ext"));
        let nfs = format!("srv:/export/{SR} /run/sr-mount/{SR} nfs4 rw 0 0\n");
        assert_eq!(kind_from_target(&t, SR, &nfs).as_deref(), Some("nfs"));
        // Local ISO SR: a directory, not a mount.
        assert_eq!(kind_from_target(&t, SR, "").as_deref(), Some("file"));
        let lv = format!("/dev/VG_XenStorage-{SR}/VHD-{VDI}");
        assert_eq!(kind_from_target(&lv, SR, "").as_deref(), Some("lvm"));
        assert_eq!(kind_from_target("/dev/drbd1000", SR, "").as_deref(), None);
        assert_eq!(kind_from_fstype("nfs"), "nfs");
        assert_eq!(kind_from_fstype("cifs"), "smb");
    }

    /// Every vbd3 backend captured from a real XCP-ng 8.3 host: what we get
    /// out of `params` must agree with what SM published in sm-data.
    #[test]
    fn real_xcpng83_dump() {
        let dump = include_str!("testdata/xcpng83-xenstore.txt");
        let kv: HashMap<&str, &str> = dump
            .lines()
            .filter_map(|l| l.split_once(" = "))
            .map(|(k, v)| (k, v.trim_matches('"')))
            .collect();
        let mut seen = 0;
        for (k, v) in &kv {
            let Some(node) = k.strip_suffix("/params") else {
                continue;
            };
            let b = parse_params(v);
            if v.is_empty() {
                assert_eq!(b, Backing::default(), "{k}");
                continue;
            }
            assert_eq!(
                b.vdi.as_deref(),
                kv.get(format!("{node}/sm-data/vdi-uuid").as_str()).copied(),
                "{k}"
            );
            assert_eq!(
                b.sr.as_deref(),
                kv.get(format!("{node}/sm-data/mem-pool").as_str()).copied(),
                "{k}"
            );
            seen += 1;
        }
        assert_eq!(seen, 12);
        for (k, v) in kv.iter().filter(|(k, _)| k.ends_with("/vm")) {
            assert!(vm_uuid(v).is_some(), "{k}");
        }
        assert_eq!(parse_kib(kv["/local/domain/16/memory/target"]), Some(8 << 30));
    }
}
