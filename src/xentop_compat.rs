//! Drop-in replacement for `xentop`'s command line and batch output.
//!
//! Active when the binary is invoked as `xentop` (symlink, busybox style) or
//! with `--xentop [xentop options]`. The options are parsed like xentop's
//! `getopt_long()` does; `-b` then prints exactly what `xentop -b` prints
//! (tools/xentop/xentop.c, Xen 4.17 up to 4.21's `-p`/`-z`), so scripts that
//! parse it keep working. Without `-b` the normal UI starts.
//!
//! Golden outputs in `tests/xentop/` were produced by the real xentop.c; see
//! `tests/xentop/harness/regen.sh`.

use crate::model::{flag, Snapshot, VbdKind};
use crate::source::Source;
use anyhow::Result;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// xentop's options. Field names follow xentop.c.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cli {
    /// Seconds between samples (`-d`, default 3; parsed with atoi()).
    pub delay: u32,
    pub delay_set: bool,
    pub batch: bool,
    pub show_networks: bool,
    pub show_vbds: bool,
    pub repeat_header: bool,
    pub show_vcpus: bool,
    pub show_pcpus: bool,
    /// `-i N`: stop after N samples. None: run until interrupted. Like
    /// xentop, 0 and negative values wrap around to (practically) forever.
    pub iterations: Option<u32>,
    pub show_full_name: bool,
    pub dom0_first: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Cli {
            delay: 3,
            delay_set: false,
            batch: false,
            show_networks: false,
            show_vbds: false,
            repeat_header: false,
            show_vcpus: false,
            show_pcpus: false,
            iterations: None,
            show_full_name: false,
            dom0_first: false,
        }
    }
}

/// Result of parsing xentop's command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Cli),
    /// Print `stdout` and `stderr`, then exit with `code` (-h, -V, errors).
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

pub fn usage(prog: &str) -> String {
    format!(
        "Usage: {prog} [OPTION]\n\
         Displays ongoing information about xen vm resources \n\n\
         -h, --help           display this help and exit\n\
         -V, --version        output version information and exit\n\
         -d, --delay=SECONDS  seconds between updates (default 3)\n\
         -n, --networks       output vif network data\n\
         -x, --vbds           output vbd block device data\n\
         -r, --repeat-header  repeat table header before each domain\n\
         -v, --vcpus          output vcpu data\n\
         -b, --batch          output in batch mode, no user input accepted\n\
         -p, --pcpus          show physical CPU stats\n\
         -i, --iterations     number of iterations before exiting\n\
         -f, --full-name      output the full domain name (not truncated)\n\
         -z, --dom0-first     display dom0 first (ignore sorting)\n\
         \n\
         Report bugs to <xen-devel@lists.xen.org>.\n"
    )
}

pub const VERSION: &str = "xentop 1.0\n\
Written by Judy Fischbach, David Hendricks, Josh Triplett\n\
\n\
Copyright (C) 2005  International Business Machines  Corp\n\
This is free software; see the source for copying conditions.There is NO\n\
warranty; not even for MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.\n";

/// xentop's long options, in its table order (glibc reports ambiguities in
/// terms of it): name, takes an argument, short equivalent.
const LONG_OPTS: [(&str, bool, char); 12] = [
    ("help", false, 'h'),
    ("version", false, 'V'),
    ("networks", false, 'n'),
    ("vbds", false, 'x'),
    ("repeat-header", false, 'r'),
    ("vcpus", false, 'v'),
    ("delay", true, 'd'),
    ("batch", false, 'b'),
    ("pcpus", false, 'p'),
    ("iterations", true, 'i'),
    ("full-name", false, 'f'),
    ("dom0-first", false, 'z'),
];
const SHORT_OPTS: &str = "hVnxrvd:bpi:fz";

/// C atoi(): optional blanks and sign, then digits; anything else stops it.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for b in digits.bytes().take_while(u8::is_ascii_digit) {
        v = (v * 10 + i64::from(b - b'0')).min(1 << 32);
    }
    (if neg { -v } else { v }) as i32
}

/// Parse xentop's arguments (without argv[0]) the way its getopt_long()
/// loop does: clustered short options (`-bi2`), attached or separate
/// values, `--long=value`, unambiguous long prefixes (`--vb`), non-options
/// skipped, `--` ends the options. `prog` is used in messages.
pub fn parse(prog: &str, args: &[String]) -> Parsed {
    let mut cli = Cli::default();
    // xentop prints the usage and exits 0 on any option error.
    let fail = |msg: String| Parsed::Exit {
        code: 0,
        stdout: usage(prog),
        stderr: format!("{prog}: {msg}\n"),
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            break;
        }
        if let Some(body) = a.strip_prefix("--") {
            let (name, val) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let exact = LONG_OPTS.iter().find(|o| o.0 == name);
            let found: Vec<_> = LONG_OPTS.iter().filter(|o| o.0.starts_with(name)).collect();
            let &(lname, has_arg, c) = match (exact, found.as_slice()) {
                (Some(o), _) => o,
                (None, [o]) => *o,
                (None, []) => return fail(format!("unrecognized option '--{name}'")),
                (None, [first, rest @ ..]) => {
                    // glibc lists the first match, then the others newest first.
                    let list: String = std::iter::once(first)
                        .chain(rest.iter().rev())
                        .map(|o| format!(" '--{}'", o.0))
                        .collect();
                    return fail(format!("option '{a}' is ambiguous; possibilities:{list}"));
                }
            };
            let val = match (has_arg, val) {
                (true, Some(v)) => Some(v),
                (true, None) if i < args.len() => {
                    i += 1;
                    Some(args[i - 1].clone())
                }
                (true, None) => return fail(format!("option '--{lname}' requires an argument")),
                (false, Some(_)) => return fail(format!("option '--{lname}' doesn't allow an argument")),
                (false, None) => None,
            };
            if let Some(exit) = apply(&mut cli, prog, c, val.as_deref()) {
                return exit;
            }
        } else if let Some(cluster) = a.strip_prefix('-').filter(|c| !c.is_empty()) {
            for (pos, c) in cluster.char_indices() {
                let Some(spec) = SHORT_OPTS.find(c).filter(|_| c != ':') else {
                    return fail(format!("invalid option -- '{c}'"));
                };
                if SHORT_OPTS[spec + c.len_utf8()..].starts_with(':') {
                    let rest = &cluster[pos + c.len_utf8()..];
                    let val = if !rest.is_empty() {
                        rest.to_string()
                    } else if i < args.len() {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        return fail(format!("option requires an argument -- '{c}'"));
                    };
                    if let Some(exit) = apply(&mut cli, prog, c, Some(&val)) {
                        return exit;
                    }
                    break;
                }
                // Options take effect in order: "-Vq" prints the version.
                if let Some(exit) = apply(&mut cli, prog, c, None) {
                    return exit;
                }
            }
        }
        // Anything else is not an option: GNU getopt skips it.
    }
    Parsed::Run(cli)
}

/// One option as xentop's switch statement handles it; Some(exit) for the
/// options that end the program.
fn apply(cli: &mut Cli, prog: &str, c: char, val: Option<&str>) -> Option<Parsed> {
    let val = val.unwrap_or("");
    match c {
        'h' | 'V' => {
            return Some(Parsed::Exit {
                code: 0,
                stdout: if c == 'h' { usage(prog) } else { VERSION.into() },
                stderr: String::new(),
            })
        }
        'n' => cli.show_networks = true,
        'x' => cli.show_vbds = true,
        'r' => cli.repeat_header = true,
        'v' => cli.show_vcpus = true,
        'd' => {
            cli.delay = atoi(val) as u32;
            cli.delay_set = true;
        }
        'b' => cli.batch = true,
        'p' => cli.show_pcpus = true,
        'i' => cli.iterations = Some(atoi(val) as u32),
        'f' => cli.show_full_name = true,
        'z' => cli.dom0_first = true,
        _ => unreachable!("option table and match out of sync"),
    }
    None
}

/// Decide between xentop-ng's own command line and xentop's. Returns
/// xentop-ng's arguments and, in xentop mode, the program name to show and
/// xentop's arguments:
/// - invoked as `xentop` (e.g. through a symlink): everything is xentop's;
/// - `xentop-ng [OPTIONS] --xentop [XENTOP OPTIONS]`: split there.
pub fn split_args(args: impl IntoIterator<Item = String>) -> (Vec<String>, Option<(String, Vec<String>)>) {
    let mut args = args.into_iter();
    let argv0 = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    let base = argv0.rsplit('/').next().unwrap_or_default();
    if base == "xentop" {
        return (Vec::new(), Some((argv0, rest)));
    }
    match rest.iter().position(|a| a == "--xentop") {
        Some(p) => (
            rest[..p].to_vec(),
            Some(("xentop".into(), rest[p + 1..].to_vec())),
        ),
        None => (rest, None),
    }
}

/// Parse xentop's options, or print help/version/errors and exit like it.
pub fn parse_or_exit(prog: &str, args: &[String]) -> Cli {
    match parse(prog, args) {
        Parsed::Run(cli) => cli,
        Parsed::Exit { code, stdout, stderr } => {
            eprint!("{stderr}");
            print!("{stdout}");
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
    }
}

impl Cli {
    /// Refresh interval for the interactive UI, if `-d` was given.
    pub fn ui_delay(&self) -> Option<Duration> {
        self.delay_set
            .then(|| Duration::from_secs_f64(f64::from(self.delay).clamp(0.1, 60.0)))
    }
}

// ---------------------------------------------------------------------------
// Batch output

/// printf("%*.1f"): like Rust's, except for how NaN is spelled.
fn f1(v: f64, w: usize) -> String {
    if v.is_nan() {
        let s = if v.is_sign_negative() { "-nan" } else { "nan" };
        format!("{s:>w$}")
    } else {
        format!("{v:>w$.1}")
    }
}

/// xentop's INT_FIELD_WIDTH(n): digits of n, computed through log10().
fn int_field_width(n: u64) -> usize {
    ((n as f64).log10() + 1.0) as usize
}

/// printf("%*s") pads by bytes, not characters; do the same.
fn pad_bytes(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(s.len())))
}

/// printf("%10.10s"): at most 10 bytes, without splitting a character.
fn trunc_bytes(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn vbd_type(k: VbdKind) -> &'static str {
    match k {
        VbdKind::Blkback => "BlkBack",
        VbdKind::Tap => "BlkTap",
        VbdKind::Vbd3 => "Tapdisk3",
        // xentop has no name for it (and reads past its table).
        VbdKind::Qdisk => "Qdisk",
        VbdKind::Unknown => "Unidentified",
    }
}

/// Column widths that grow with the data (xentop's `default_width`).
struct Widths {
    name: usize,
    net_tx: usize,
    net_rx: usize,
    vbd_rd: usize,
    vbd_wr: usize,
    vbd_rsect: usize,
    vbd_wsect: usize,
}

/// One domain's totals, as xentop computes them.
struct DomTotals {
    net_tx: u64,
    net_rx: u64,
    /// OO, RD, WR, RSECT, WSECT summed over the VBDs without an error;
    /// None when every VBD is in error.
    vbd: Option<[u64; 5]>,
}

fn totals(d: &crate::model::DomainRaw) -> DomTotals {
    let net_tx = d.nets.iter().fold(0u64, |a, n| a.wrapping_add(n.tbytes));
    let net_rx = d.nets.iter().fold(0u64, |a, n| a.wrapping_add(n.rbytes));
    let ok: Vec<_> = d.vbds.iter().filter(|v| !v.error).collect();
    let vbd = (d.vbds.is_empty() || !ok.is_empty()).then(|| {
        ok.iter().fold([0u64; 5], |a, v| {
            [
                a[0].wrapping_add(v.oo_reqs),
                a[1].wrapping_add(v.rd_reqs),
                a[2].wrapping_add(v.wr_reqs),
                a[3].wrapping_add(v.rd_sects),
                a[4].wrapping_add(v.wr_sects),
            ]
        })
    });
    DomTotals { net_tx, net_rx, vbd }
}

/// Formats samples exactly like `xentop -b`. Keeps the state xentop keeps
/// between iterations (the `-f` name column only ever grows).
pub struct Printer {
    cli: Cli,
    name_width: usize,
}

impl Printer {
    pub fn new(cli: &Cli) -> Self {
        Printer {
            cli: cli.clone(),
            name_width: 10,
        }
    }

    /// One iteration's output for sample `cur`; `prev` is the previous
    /// sample (None on the first iteration: CPU(%) is then 0.0).
    pub fn frame(&mut self, prev: Option<&Snapshot>, cur: &Snapshot) -> String {
        let mut out = String::new();
        // Microseconds between the samples, as xentop's gettimeofday() diff.
        let us = prev.map(|p| cur.at.saturating_duration_since(p.at).as_micros() as u64);

        // Sort by name, case-insensitively (xentop's default sort column);
        // -z swaps Domain-0 to the front and leaves it out of the sort.
        let mut doms: Vec<_> = cur.domains.iter().collect();
        let mut start = 0;
        if self.cli.dom0_first {
            if let Some(i) = doms.iter().rposition(|d| d.name == "Domain-0") {
                doms.swap(0, i);
                start = 1;
            }
        }
        doms[start..].sort_by(|a, b| {
            let lower = |s: &str| s.bytes().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
            lower(&a.name).cmp(&lower(&b.name))
        });

        let tot: Vec<DomTotals> = doms.iter().map(|d| totals(d)).collect();
        let mut w = Widths {
            name: 10,
            net_tx: 8,
            net_rx: 8,
            vbd_rd: 8,
            vbd_wr: 8,
            vbd_rsect: 10,
            vbd_wsect: 10,
        };
        for (d, t) in doms.iter().zip(&tot) {
            if self.cli.show_full_name {
                self.name_width = self.name_width.max(d.name.len());
            }
            let grow = |w: &mut usize, v: u64| *w = (*w).max(int_field_width(v.wrapping_add(1)));
            grow(&mut w.net_tx, t.net_tx / 1024);
            grow(&mut w.net_rx, t.net_rx / 1024);
            let v = t.vbd.unwrap_or_default();
            grow(&mut w.vbd_rd, v[1]);
            grow(&mut w.vbd_wr, v[2]);
            grow(&mut w.vbd_rsect, v[3]);
            grow(&mut w.vbd_wsect, v[4]);
        }
        if self.cli.show_full_name {
            w.name = self.name_width;
        }

        let header = [
            ("NAME", w.name),
            ("STATE", 6),
            ("CPU(sec)", 10),
            ("CPU(%)", 6),
            ("MEM(k)", 10),
            ("MEM(%)", 6),
            ("MAXMEM(k)", 10),
            ("MAXMEM(%)", 9),
            ("VCPUS", 5),
            ("NETS", 4),
            ("NETTX(k)", w.net_tx),
            ("NETRX(k)", w.net_rx),
            ("VBDS", 4),
            ("VBD_OO", 8),
            ("VBD_RD", w.vbd_rd),
            ("VBD_WR", w.vbd_wr),
            ("VBD_RSECT", w.vbd_rsect),
            ("VBD_WSECT", w.vbd_wsect),
            ("SSID", 4),
        ]
        .iter()
        .map(|&(h, w)| format!("{h:>w$}"))
        .collect::<Vec<_>>()
        .join(" ");

        let tot_mem = cur.tot_mem as f64;
        for (n, (d, t)) in doms.iter().zip(&tot).enumerate() {
            if n == 0 || self.cli.repeat_header {
                out += &header;
                out.push('\n');
            }
            let name = if self.cli.show_full_name {
                pad_bytes(&d.name, w.name)
            } else {
                pad_bytes(trunc_bytes(&d.name, 10), 10)
            };
            let state: String = [
                (flag::DYING, 'd'),
                (flag::SHUTDOWN, 's'),
                (flag::BLOCKED, 'b'),
                (flag::CRASHED, 'c'),
                (flag::PAUSED, 'p'),
                (flag::RUNNING, 'r'),
            ]
            .iter()
            .map(|&(f, c)| if d.flags & f != 0 { c } else { '-' })
            .collect();
            let cpu_pct = match (prev.and_then(|p| p.domains.iter().find(|x| x.id == d.id)), us) {
                (Some(old), Some(us)) if us > 0 => {
                    d.cpu_ns.saturating_sub(old.cpu_ns) as f64 / 10.0 / us as f64
                }
                _ => 0.0,
            };
            let (maxmem, maxpct) = if d.max_mem == u64::MAX {
                ("no limit".to_string(), "n/a".to_string())
            } else {
                (
                    (d.max_mem / 1024).to_string(),
                    f1(d.max_mem as f64 / tot_mem * 100.0, 9),
                )
            };
            let vbd = |i: usize, w: usize| match t.vbd {
                Some(v) => format!("{:>w$}", v[i]),
                None => format!("{:>w$}", '-'),
            };
            out += &format!(
                "{name} {state} {:>10} {} {:>10} {} {maxmem:>10} {maxpct:>9} {:>5} {:>4} {:>ntx$} {:>nrx$} {:>4} {} {} {} {} {} {:>4}\n",
                d.cpu_ns / 1_000_000_000,
                f1(cpu_pct, 6),
                d.cur_mem / 1024,
                f1(d.cur_mem as f64 / tot_mem * 100.0, 6),
                d.vcpus.len(),
                d.nets.len(),
                t.net_tx / 1024,
                t.net_rx / 1024,
                d.vbds.len(),
                vbd(0, 8),
                vbd(1, w.vbd_rd),
                vbd(2, w.vbd_wr),
                vbd(3, w.vbd_rsect),
                vbd(4, w.vbd_wsect),
                d.ssid,
                ntx = w.net_tx,
                nrx = w.net_rx,
            );

            if self.cli.show_vcpus {
                out += "VCPUs(sec): ";
                for (i, v) in d.vcpus.iter().enumerate().filter(|(_, v)| v.online) {
                    if i != 0 && i % 5 == 0 {
                        out += "\n        ";
                    }
                    out += &format!(" {i:>2}: {:>10}s", v.ns / 1_000_000_000);
                }
                out.push('\n');
            }
            if self.cli.show_networks {
                for (i, x) in d.nets.iter().enumerate() {
                    out += &format!(
                        "Net{i} RX: {:>8}bytes {:>8}pkts {:>8}err {:>8}drop  \
                         TX: {:>8}bytes {:>8}pkts {:>8}err {:>8}drop\n",
                        x.rbytes, x.rpackets, x.rerrs, x.rdrop, x.tbytes, x.tpackets, x.terrs, x.tdrop
                    );
                }
            }
            if self.cli.show_vbds {
                for v in &d.vbds {
                    let head = format!(
                        "VBD {} {:>4} [{:>2x}:{:>2x}] ",
                        vbd_type(v.kind),
                        v.dev as i32,
                        v.dev >> 8,
                        v.dev & 0xff
                    );
                    out += &if v.error {
                        format!(
                            "{head} OO: {:>8}   RD: {:>8}   WR: {:>8}   RSECT: {:>10}   WSECT: {:>10}\n",
                            '-', '-', '-', '-', '-'
                        )
                    } else {
                        format!(
                            "{head} OO: {:>8}   RD: {:>8}   WR: {:>8}   RSECT: {:>10}   WSECT: {:>10}\n",
                            v.oo_reqs, v.rd_reqs, v.wr_reqs, v.rd_sects, v.wr_sects
                        )
                    };
                }
            }
        }

        if self.cli.show_pcpus {
            self.pcpus(prev, cur, us, &mut out);
        }
        out
    }

    /// `-p`: per-pCPU usage table (xentop's pcpu.c). Cores are labelled
    /// with their CPU id; offline ones are left out.
    fn pcpus(&self, prev: Option<&Snapshot>, cur: &Snapshot, us: Option<u64>, out: &mut String) {
        let Some(idle) = cur.pcpu_idle_ns.as_ref().filter(|v| !v.is_empty()) else {
            *out += "\nNo PCPU data available\n";
            return;
        };
        *out += "\nPhysical CPU Usage:\n+-------+--------+\n| Core  | Usage  |\n+-------+--------+\n";
        let before = prev.and_then(|p| p.pcpu_idle_ns.as_ref());
        for &(id, ns) in idle {
            let old = before.and_then(|b| b.iter().find(|x| x.0 == id)).map(|x| x.1);
            let usage = match (old, us) {
                (Some(old), Some(us)) if us > 0 => {
                    // Same arithmetic as pcpu.c, in µs, with a float result.
                    let idle_us = (ns / 1000).wrapping_sub(old / 1000);
                    let u = 100.0 * (1.0 - idle_us as f64 / us as f64);
                    u.clamp(0.0, 100.0) as f32
                }
                _ => 0.0,
            };
            *out += &format!("| {id:<5} | {:>5.1}% |\n", f64::from(usage));
        }
        *out += "+-------+--------+\n";
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The batch loop: sample, print, sleep. `sleep` returns false to stop
/// (interrupted). Split out from `batch` so tests can drive it.
pub fn run_batch(
    cli: &Cli,
    mut sample: impl FnMut() -> Result<Snapshot>,
    out: &mut impl Write,
    mut sleep: impl FnMut(u32) -> bool,
) -> Result<()> {
    let mut printer = Printer::new(cli);
    let mut prev: Option<Snapshot> = None;
    let mut left = cli.iterations;
    loop {
        let cur = sample()?;
        out.write_all(printer.frame(prev.as_ref(), &cur).as_bytes())?;
        out.flush()?;
        prev = Some(cur);
        if let Some(n) = left.as_mut() {
            *n = n.wrapping_sub(1);
            if *n == 0 {
                return Ok(());
            }
        }
        if !sleep(cli.delay) {
            return Ok(());
        }
    }
}

/// `xentop -b`: print until the iteration count is reached or SIGINT/SIGTERM
/// arrives (then exit 0 after the current iteration, like xentop).
pub fn batch(mut src: Box<dyn Source>, cli: &Cli) -> Result<()> {
    // SAFETY: plain signal(2)/sigaction(2) calls; the handler only stores to
    // an atomic. No SA_RESTART, so sleep() returns early on a signal.
    unsafe {
        // Like a C program: die quietly when the reader goes away.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
    let mut out = std::io::stdout().lock();
    run_batch(
        cli,
        || src.sample(),
        &mut out,
        |secs| {
            if STOP.load(Ordering::Relaxed) {
                return false;
            }
            // SAFETY: sleep(3) has no preconditions.
            unsafe { libc::sleep(secs) };
            !STOP.load(Ordering::Relaxed)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DomState, DomainRaw, NetRaw, VbdRaw, VcpuRaw};
    use std::time::Instant;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn run(s: &str) -> Cli {
        match parse("xentop", &args(s)) {
            Parsed::Run(c) => c,
            e => panic!("{s}: {e:?}"),
        }
    }

    fn exit(s: &str) -> (i32, String, String) {
        match parse("xentop", &args(s)) {
            Parsed::Exit { code, stdout, stderr } => (code, stdout, stderr),
            r => panic!("{s}: {r:?}"),
        }
    }

    #[test]
    fn parses_like_getopt_long() {
        let c = run("-b -i 2 -d 1");
        assert!(c.batch && c.delay_set);
        assert_eq!((c.iterations, c.delay), (Some(2), 1));
        assert_eq!(run(""), Cli::default());
        assert_eq!(Cli::default().delay, 3);

        // Clusters, attached values, long options and their prefixes.
        let c = run("-bfi3 -d2 --net --vb --repeat --vc --full --dom0 --p");
        assert!(c.batch && c.show_full_name && c.show_networks && c.show_vbds);
        assert!(c.repeat_header && c.show_vcpus && c.dom0_first && c.show_pcpus);
        assert_eq!((c.iterations, c.delay), (Some(3), 2));
        assert_eq!(run("--iterations=4 --delay 5").iterations, Some(4));
        assert_eq!(run("--iterations=4 --delay 5").delay, 5);
        // A value is taken even when it looks like an option.
        assert_eq!(run("-i -5").iterations, Some(-5i32 as u32));
        assert_eq!(run("-d -b").delay, 0);
        assert!(!run("-d -b").batch);

        // Non-options are skipped; "--" ends the options.
        let c = run("stray -b more -- -v");
        assert!(c.batch && !c.show_vcpus);

        // atoi(): "0.5" is 0, junk is 0, trailing junk ignored.
        assert_eq!(run("-d 0.5").delay, 0);
        assert_eq!(run("-d abc").delay, 0);
        assert_eq!(run("-i 7x").iterations, Some(7));
        assert_eq!(run("-i 0").iterations, Some(0));
    }

    #[test]
    fn help_version_and_errors_exit_like_xentop() {
        let (code, out, err) = exit("-h");
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.starts_with("Usage: xentop [OPTION]\n"));
        assert!(out.contains("-z, --dom0-first     display dom0 first (ignore sorting)\n"));
        assert_eq!(exit("--vers").1, VERSION);
        assert_eq!(exit("-bV").1, VERSION);
        // Options act in order: the first one that exits wins, even over a
        // later error in the same cluster.
        assert_eq!(exit("-V -h").1, VERSION);
        assert_eq!(exit("-bVq"), (0, VERSION.into(), String::new()));
        assert_eq!(exit("-qV").2, "xentop: invalid option -- 'q'\n");

        for (a, msg) in [
            ("-q", "xentop: invalid option -- 'q'\n"),
            ("-b -d", "xentop: option requires an argument -- 'd'\n"),
            ("--bogus", "xentop: unrecognized option '--bogus'\n"),
            (
                "--batch=1",
                "xentop: option '--batch' doesn't allow an argument\n",
            ),
            ("--delay", "xentop: option '--delay' requires an argument\n"),
            (
                "--v",
                "xentop: option '--v' is ambiguous; possibilities: '--version' '--vcpus' '--vbds'\n",
            ),
            (
                "--d",
                "xentop: option '--d' is ambiguous; possibilities: '--delay' '--dom0-first'\n",
            ),
        ] {
            let (code, out, err) = exit(a);
            assert_eq!(code, 0, "{a}");
            assert_eq!(err, msg, "{a}");
            assert_eq!(out, usage("xentop"), "{a}");
        }
    }

    #[test]
    fn usage_matches_xentop() {
        // tools/xentop/xentop.c usage(), Xen 4.21.
        let expected =
            "Usage: /usr/sbin/xentop [OPTION]\nDisplays ongoing information about xen vm resources\x20

-h, --help           display this help and exit
-V, --version        output version information and exit
-d, --delay=SECONDS  seconds between updates (default 3)
-n, --networks       output vif network data
-x, --vbds           output vbd block device data
-r, --repeat-header  repeat table header before each domain
-v, --vcpus          output vcpu data
-b, --batch          output in batch mode, no user input accepted
-p, --pcpus          show physical CPU stats
-i, --iterations     number of iterations before exiting
-f, --full-name      output the full domain name (not truncated)
-z, --dom0-first     display dom0 first (ignore sorting)

Report bugs to <xen-devel@lists.xen.org>.
";
        assert_eq!(usage("/usr/sbin/xentop"), expected);
    }

    #[test]
    fn splits_command_lines() {
        let v = |s: &str| args(s);
        // Our own command line is untouched.
        assert_eq!(split_args(v("xentop-ng -d 2 --demo")), (v("-d 2 --demo"), None));
        assert_eq!(split_args(v("/opt/x/bin/xentop-ng -b")), (v("-b"), None));
        // Invoked as xentop: everything belongs to xentop.
        assert_eq!(
            split_args(v("/usr/local/sbin/xentop -b --xentop -i 1")),
            (
                vec![],
                Some(("/usr/local/sbin/xentop".into(), v("-b --xentop -i 1")))
            )
        );
        assert_eq!(split_args(v("xentop")), (vec![], Some(("xentop".into(), vec![]))));
        // Explicit switch: ours before, xentop's after.
        assert_eq!(
            split_args(v("xentop-ng --demo --xentop -b -i 1")),
            (v("--demo"), Some(("xentop".into(), v("-b -i 1"))))
        );
        // Not fooled by look-alikes.
        assert_eq!(split_args(v("xentop-ng-dev -b")).1, None);
        assert_eq!(split_args(v("/home/xentop/xentop-ng -b")).1, None);
    }

    #[test]
    fn ui_delay_mapping() {
        assert_eq!(run("-b").ui_delay(), None);
        assert_eq!(run("-d 5").ui_delay(), Some(Duration::from_secs(5)));
        assert_eq!(run("-d 0").ui_delay(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn float_and_width_helpers_match_printf() {
        // glibc rounds exact ties to even, as Rust does.
        assert_eq!(f1(0.25, 6), "   0.2");
        assert_eq!(f1(0.35, 6), "   0.3");
        assert_eq!(f1(12.25, 6), "  12.2");
        assert_eq!(f1(-0.0, 6), "  -0.0");
        assert_eq!(f1(f64::INFINITY, 6), "   inf");
        assert_eq!(f1(-f64::NAN, 6), "  -nan");
        assert_eq!(int_field_width(1), 1);
        assert_eq!(int_field_width(99_999_999), 8);
        // xentop sizes columns on value + 1.
        assert_eq!(int_field_width(100_000_000), 9);
        assert_eq!(int_field_width(0), 0);
        assert_eq!(trunc_bytes("ÉtéVM-ünïcode", 10), "ÉtéVM-ü");
        // Never split a character: "é" would straddle the 10th byte.
        assert_eq!(trunc_bytes("abcdefghié", 10), "abcdefghi");
        assert_eq!(pad_bytes("abcdefghi", 10), " abcdefghi");
    }

    // -- Golden tests --------------------------------------------------------

    /// Parse a fixture (format in tests/xentop/harness/stub.c).
    fn load_fixture(text: &str) -> Vec<Snapshot> {
        let base = Instant::now();
        let mut snaps: Vec<Snapshot> = Vec::new();
        let num = |s: &str| s.parse::<u64>().unwrap();
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let (kind, rest) = line.split_once(' ').unwrap();
            let f: Vec<&str> = rest.split(' ').collect();
            if kind == "node" {
                snaps.push(Snapshot {
                    at: base + Duration::from_micros(num(f[0])),
                    hostname: "fixture".into(),
                    xen_version: f[5].into(),
                    num_cpus: num(f[3]) as u32,
                    cpu_hz: num(f[4]),
                    tot_mem: num(f[1]),
                    free_mem: num(f[2]),
                    pcpu_idle_ns: None,
                    domains: vec![],
                    host_srs: Vec::new(),
                });
                continue;
            }
            let s = snaps.last_mut().unwrap();
            match kind {
                "pcpu" => {
                    let v = s.pcpu_idle_ns.get_or_insert_with(Vec::new);
                    v.push((v.len() as u32, num(f[0])));
                }
                "dom" => {
                    let flags = [
                        ('d', flag::DYING),
                        ('s', flag::SHUTDOWN),
                        ('b', flag::BLOCKED),
                        ('c', flag::CRASHED),
                        ('p', flag::PAUSED),
                        ('r', flag::RUNNING),
                    ]
                    .iter()
                    .filter(|(c, _)| f[1].contains(*c))
                    .fold(0, |a, (_, b)| a | b);
                    s.domains.push(DomainRaw {
                        id: num(f[0]) as u32,
                        name: f[6..].join(" "),
                        state: DomState::Blocked,
                        flags,
                        ssid: num(f[5]) as u32,
                        vm_uuid: None,
                        mem_target: None,
                        runnable_ns: None,
                        cpu_ns: num(f[2]),
                        vcpus: vec![],
                        cur_mem: num(f[3]),
                        max_mem: num(f[4]),
                        nets: vec![],
                        vbds: vec![],
                    });
                }
                "vcpu" => s.domains.last_mut().unwrap().vcpus.push(VcpuRaw {
                    online: f[0] != "0",
                    ns: num(f[1]),
                    runnable_ns: None,
                    runstate_at_ns: None,
                }),
                "net" => {
                    let n: Vec<u64> = f.iter().map(|x| num(x)).collect();
                    s.domains.last_mut().unwrap().nets.push(NetRaw {
                        id: n[0] as u32,
                        network: None,
                        rbytes: n[1],
                        rpackets: n[2],
                        rerrs: n[3],
                        rdrop: n[4],
                        tbytes: n[5],
                        tpackets: n[6],
                        terrs: n[7],
                        tdrop: n[8],
                    });
                }
                "vbd" => {
                    let n: Vec<u64> = f.iter().map(|x| num(x)).collect();
                    s.domains.last_mut().unwrap().vbds.push(VbdRaw {
                        dev: n[1] as u32,
                        kind: VbdKind::from_xenstat(n[0] as u32),
                        oo_reqs: n[3],
                        rd_reqs: n[4],
                        wr_reqs: n[5],
                        rd_sects: n[6],
                        wr_sects: n[7],
                        error: n[2] != 0,
                        connecting: false,
                        ext: None,
                        backing: None,
                    });
                }
                k => panic!("fixture: unknown record {k}"),
            }
        }
        snaps
    }

    fn fixture_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/xentop")
    }

    /// Run the batch loop over a fixture with xentop's arguments.
    fn render(fixture: &str, xentop_args: &[String]) -> String {
        let text = std::fs::read_to_string(fixture_dir().join(fixture)).unwrap();
        let mut snaps = load_fixture(&text).into_iter();
        let Parsed::Run(cli) = parse("xentop", xentop_args) else {
            panic!("bad args {xentop_args:?}");
        };
        let mut out = Vec::new();
        run_batch(
            &cli,
            || snaps.next().ok_or_else(|| anyhow::anyhow!("fixture exhausted")),
            &mut out,
            |_| true,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Every case in tests/xentop/cases.txt must reproduce, byte for byte,
    /// what the real xentop printed for the same samples.
    #[test]
    fn golden_outputs_match_real_xentop() {
        let cases = std::fs::read_to_string(fixture_dir().join("cases.txt")).unwrap();
        let mut n = 0;
        for line in cases
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let f = args(line);
            let expected = std::fs::read_to_string(fixture_dir().join(format!("{}.out", f[0]))).unwrap();
            let got = render(&f[1], &f[2..]);
            if got != expected {
                for (i, (g, e)) in got.lines().zip(expected.lines()).enumerate() {
                    assert_eq!(g, e, "{}: first difference on line {}", f[0], i + 1);
                }
                assert_eq!(got, expected, "{}", f[0]);
            }
            n += 1;
        }
        assert!(n >= 9, "only {n} golden cases");
    }

    /// The fixture mirrors a real XCP-ng 8.3 host; our output for it must be
    /// identical to what xentop printed on that host.
    #[test]
    fn matches_capture_from_real_host() {
        let captured =
            std::fs::read_to_string(fixture_dir().join("captured/xcpng-8.3-b-i1-n-x-v.txt")).unwrap();
        assert_eq!(render("xcpng-host.snap", &args("-b -i 1 -n -x -v")), captured);
    }

    #[test]
    fn no_domains_prints_no_table() {
        let mut s = load_fixture("node 1 1024 0 1 0 4.17\npcpu 5\n").remove(0);
        let mut p = Printer::new(&run("-b -p"));
        assert_eq!(
            p.frame(None, &s),
            "\nPhysical CPU Usage:\n+-------+--------+\n| Core  | Usage  |\n+-------+--------+\n\
             | 0     |   0.0% |\n+-------+--------+\n"
        );
        s.pcpu_idle_ns = None;
        assert_eq!(p.frame(None, &s), "\nNo PCPU data available\n");
        assert_eq!(Printer::new(&run("-b")).frame(None, &s), "");
    }

    #[test]
    fn qdisk_and_counter_resets_stay_sane() {
        let text = "node 0 1048576 0 1 0 4.17\n\
                    dom 1 r 5000000000 1024 1024 0 vm\nvbd 4 51712 0 0 0 0 0 0\n\
                    node 1000000 1048576 0 1 0 4.17\n\
                    dom 1 r 1000000000 1024 1024 0 vm\nvbd 4 51712 0 0 0 0 0 0\n";
        let s = load_fixture(text);
        let mut p = Printer::new(&run("-b -x"));
        p.frame(None, &s[0]);
        let out = p.frame(Some(&s[0]), &s[1]);
        // Domain ID reused / counter went backwards: 0.0, not a wrapped
        // huge value.
        assert!(out
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("        vm -----r          1    0.0"));
        assert!(out.contains("VBD Qdisk 51712 [ca: 0]  OO:"));
    }
}
