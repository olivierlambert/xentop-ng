# libxenstat patches

xentop-ng reads everything through libxenstat. These patches add the
metrics it needs, and fix one existing bug, so the extra data can go
upstream to Xen and help every libxenstat user, not only this tool.

Most of them only touch libxenstat. The exception is steal time: Xen has
no way for dom0 to read another domain's vCPU runstate times, so **0003
adds one to the hypervisor** (and libxenctrl), and 0004 uses it in
libxenstat.

| Patch | Touches | To get the data |
|---|---|---|
| 0001, 0002, 0005 | libxenstat | replace libxenstat (no reboot) |
| 0003 | **hypervisor**, public headers, libxenctrl | rebuild Xen, install it, **reboot** |
| 0004 | libxenstat | replace libxenstat; per-vCPU steal shows up once the hypervisor has 0003 |

| Directory | Base | Used for |
|---|---|---|
| [`xcp-ng-4.17/`](xcp-ng-4.17/) | Xen 4.17.6 + XCP-ng's patch queue (`xen-4.17.6-12.3.xcpng8.3`) | building the drop-in `libxenstat.so.4.17` for XCP-ng 8.3 (`build/build-libxenstat.sh`) |
| [`upstream/`](upstream/) | xen.git `master` | submission to xen-devel |

Both sets make the same changes. `git apply --check` passes on master for
`upstream/0001`…`0004` applied in sequence; the patched hypervisor
(`make -C xen` with the default x86 config, and again with FLASK enabled)
and `tools/libs/{ctrl,stat}` build. The 4.17 set applies with `--fuzz=0`
after XCP-ng's patch queue, and its hypervisor builds with XCP-ng's
`config-release` (and with FLASK enabled).

`build/build-libxenstat.sh` applies all of `xcp-ng-4.17/0*.patch`: 0003 is
needed there for the public header, even though only the libraries are
built. Its hypervisor and libxenctrl changes take effect only once XCP-ng's
Xen package is rebuilt with it (add it to the patch list in `xen.spec`).

## Compatibility and fallback

The patches only **add** exported functions and append fields to private
structures. The soname and existing ABI are unchanged, so the patched library
drops in for the stock one: the system `xentop` keeps working against it.

xentop-ng looks up every new symbol at runtime and treats it as optional. With
a stock library it still runs, and the matching fields show `-`. See the
table in the [main README](../README.md#stock-libxenstat-fallbacks-and-our-patches).

The same holds the other way round for 0003/0004: a patched libxenstat on a
stock hypervisor makes one failing domctl per handle, then stops asking;
`xenstat_vcpu_has_runstate()` returns 0. The new domctl does not change
`XEN_DOMCTL_INTERFACE_VERSION`, so stock tools keep working on a patched
hypervisor.

## 0001: extended VBD3 stats and per-pCPU idle time

New API:

```c
/* VBD3 (tapdisk3) only: 1 if the fields below are valid, else 0 */
unsigned int       xenstat_vbd_has_ext(xenstat_vbd *vbd);
unsigned long long xenstat_vbd_rd_reqs_done(xenstat_vbd *vbd);  /* completed reads */
unsigned long long xenstat_vbd_wr_reqs_done(xenstat_vbd *vbd);  /* completed writes */
unsigned long long xenstat_vbd_rd_usecs(xenstat_vbd *vbd);      /* cumulative service time, µs */
unsigned long long xenstat_vbd_wr_usecs(xenstat_vbd *vbd);
unsigned long long xenstat_vbd_io_errors(xenstat_vbd *vbd);

/* cumulative idle ns of physical CPU `cpu` (xc_getcpuinfo); 0 if offline/unknown */
unsigned long long xenstat_node_pcpu_idle_ns(xenstat_node *node, unsigned int cpu);
unsigned int       xenstat_node_num_pcpu_idle(xenstat_node *node);   /* max_cpu_id + 1 */
```

- **VBD fields**: tapdisk3 already writes these counters to its shared-memory
  stats file. libxenstat read the file but threw them away. Average latency
  over an interval is Δusecs / Δreqs_done.
- **pCPU idle time**: one extra `xc_getcpuinfo()` hypercall per
  `xenstat_get_node()` call.
- **Also fixed**: an on-stack `xenstat_vbd` used to be left partially
  uninitialised.

### Caveat: no in-flight count

`submitted − completed` looks like an in-flight count, but it isn't one.
tapdisk counts an empty flush (a `WRITE_BARRIER` with no segments) as a
submitted write and never as a completed one, so the difference grows
forever: we measured 3216 on an idle guest. xentop-ng doesn't display it. A
real in-flight gauge needs a small tapdisk change (see
[IDEAS.md](../IDEAS.md)).

## 0002: VIFs missing on Open vSwitch hosts

`xenstat_collect_networks()` looks for a Linux bridge so that, with bonding,
dom0 can report the bridge's counters. Open vSwitch hosts, which includes
every XCP-ng host by default, have no Linux bridge, so the bridge name stays
empty.

- `strstr(iface, "")` matches every interface, so each VIF takes the bridge
  branch and is never attached to its domain.
- Every domain ends up with zero networks, and stock `xentop` shows `NETS 0`.

The fix takes the bridge branch only when a bridge was actually found.

## 0003 and 0004: steal time

Today a vCPU's runstate times (`RUNSTATE_running`, `_runnable`, `_blocked`,
`_offline`) are visible only to its own domain (`VCPUOP_get_runstate_info`,
or a registered runstate area). Nothing in domctl, sysctl, hypfs or
libxenstat gives them to dom0: `XEN_DOMCTL_getvcpuinfo` returns the running
time and the instantaneous `blocked`/`running` flags only.

**0003, hypervisor** (`xen/common/domctl.c`, `public/domctl.h`, FLASK):

```c
#define XEN_DOMCTL_get_vcpu_runstate 91
struct xen_domctl_vcpu_runstate {
    uint32_t vcpu;                     /* IN */
    uint32_t state;                    /* OUT: current RUNSTATE_* */
    uint64_aligned_t state_entry_time; /* OUT: system time, ns */
    uint64_aligned_t time[4];          /* OUT: ns in each RUNSTATE_* */
    uint64_aligned_t sample_time;      /* OUT: system time time[] runs to */
};
```

It returns one vCPU's runstate times, brought up to `sample_time`, which
is read inside the same consistent (seqcount) snapshot through a new
`vcpu_runstate_snapshot()`. Rates must be computed over differences in
`sample_time`, not over the caller's own clock: the caller can be
descheduled between the hypercall and reading its clock, which is exactly
when the host is contended, so pairing the times with its clock skews
steal (even past 100%). The earlier version of this patch didn't return
`sample_time`; it is the last field, so a hypervisor with that version
leaves it at the 0 the caller passes in. It is handled like `XEN_DOMCTL_getvcpuinfo`
(read-only scheduler accounting, outside the domctl lock on master) and
uses the same XSM permission, `domain:getvcpuinfo`. libxenctrl gets a
wrapper:

```c
typedef struct xen_domctl_vcpu_runstate xc_vcpu_runstate_t;
int xc_vcpu_get_runstate(xc_interface *xch, uint32_t domid, uint32_t vcpu,
                         xc_vcpu_runstate_t *info);
```

**0004, libxenstat** collects it for every vCPU when `XENSTAT_VCPU` is set:

```c
/* 1 if the hypervisor provided runstate times for this VCPU, else 0 */
unsigned int       xenstat_vcpu_has_runstate(xenstat_vcpu *vcpu);
unsigned long long xenstat_vcpu_runnable_ns(xenstat_vcpu *vcpu); /* steal time */
unsigned long long xenstat_vcpu_blocked_ns(xenstat_vcpu *vcpu);
unsigned long long xenstat_vcpu_offline_ns(xenstat_vcpu *vcpu);
unsigned long long xenstat_vcpu_running_ns(xenstat_vcpu *vcpu);  /* same snapshot */
/* Hypervisor time (ns) the times above run to; 0 if not reported */
unsigned long long xenstat_vcpu_runstate_time_ns(xenstat_vcpu *vcpu);
```

`xenstat_vcpu_ns()` keeps returning running time from `xc_vcpu_getinfo()`,
read at a slightly different moment; `xenstat_vcpu_running_ns()` comes from
the same snapshot as the other times. xentop-ng computes steal over
differences in `xenstat_vcpu_runstate_time_ns()` when both samples have it,
and over its own clock otherwise. The cost is one more hypercall per vCPU
per sample, next to the existing `xc_vcpu_getinfo()`.

**XCP-ng differences**:

- The 4.17 version of 0004 issues the domctl with `xc_domctl()` instead of
  `xc_vcpu_get_runstate()`, so the bundled libxenstat still loads against
  the stock XCP-ng libxenctrl.
- XCP-ng (and XenServer) already carry `XEN_DOMCTL_get_runstate_info` (98),
  a whole-domain runstate used by xcp-rrdd. Its `runnable` field is the
  average over the domain's vCPUs, so it gives per-domain but not per-vCPU
  steal. xentop-ng uses it as a fallback through libxenctrl's
  `xc_get_runstate_info_ext()` when 0003 isn't there. 0003 takes the next
  upstream number (91), unused in 4.17, and leaves 98 alone.

**Not tested on hardware yet**: it compiles, but running it needs a host
booted on a rebuilt hypervisor.

## Before submitting upstream

- Add your `Signed-off-by:` (DCO) to each patch.
- 0003 is a hypervisor change: send it to the scheduler/domctl maintainers
  with 0004 as its user. Questions reviewers may raise: a batched form (all
  vCPUs of a domain in one call, via a guest handle) instead of one call
  per vCPU, and whether to bump `XEN_DOMCTL_INTERFACE_VERSION` in this
  release cycle (not needed for a new sub-op, but customary for some).
- Consider exporting a version marker (e.g. `xenstat_ext_version()`), so
  consumers can tell the semantics apart if the accessors change during
  review.


## 0005: safe tapdisk stats reads

Both patch sets now validate numeric PIDs and open stats relative to verified
root-owned directory descriptors. Symlinks, writable files/directories, hard
links, non-regular files and invalid sizes are rejected; nonblocking opens
prevent FIFOs from hanging the collector. Run `python3 tests/libxenstat/check-reader.py`
to exercise the reader without a Xen host. Stock libraries must be updated
separately: Rust fallback checks cannot protect reads already made by libxenstat.

Read-only validation on 2026-10-02 checked an XCP-ng 8.3 host running
`xcp-ng-release-8.3.0-37` and `blktap-3.55.5-6.7.xcpng8.3`. Its seven
`td3-*` directories were root-owned mode `0700`; all six current `vbd-*`
records were root-owned regular files, mode `0600`, with one link. All six
also passed the patch's size and version checks. This validates the policy
against those actual files; the patched library was not deployed for this check.
