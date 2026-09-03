# The assertion vocabulary

Seven kinds, closed. Every expectation carries `name` (unique per kind — it
is the alert rule slug, `mount:var-tmp-scratch`), `severity`
(`info`/`warning` default/`critical`), optional `for_secs` (debounce
override; the set-wide `default_for_secs` is 0 — host state does not flap,
and a debounce only delays the page) and optional `recover_after_secs`
(recovery hold, #932; set-wide `default_recover_after_secs`, also 0).

The two are not symmetric here. A debounce delays the page and host state
rarely flaps, so 0 is right. A **recovery** hold has a use the debounce does
not: a mount that comes back and goes again, or a listener restarting, produces
a resolved/firing pair per sweep without one. It is still 0 by default, because
holding a resolution is also a lie about the present.

**The one rule above the kinds: an unreadable observation is NOT
satisfied.** A `stat()` refused by EPERM proves nothing; those violations
carry `check=unreadable` so an operator can tell "broken" from "blind".

| Kind | Fields | Semantics |
|---|---|---|
| `mounts` | `path`, `is_bind_of?`, `fstype?`, `options[]` | `path` must be a mount point (the *visible* one — overmounts resolve to the last mountinfo entry). `is_bind_of` computes the expected `(device, root)` through the mount containing the source — btrfs subvolume roots included — and compares both. Options match against mount **or** superblock options. |
| `files` | `path`, `exists=true`, `newer_than_secs?`, `size_within_pct_of_previous?` | Freshness is max mtime age (future mtimes saturate to 0 — clock skew is not staleness). The size check latches the last size that **passed** as its baseline, so a halved backup stays firing rather than self-resolving one sweep later; baselines are in-memory (a restart reseeds on first observation) and are dropped when their rule leaves the set. |
| `listening` | `port`, `addr?`, `forbid` | TCP listeners in **this network namespace** (`/proc/net/tcp{,6}`, LISTEN only; v4-mapped v6 normalizes to v4). `addr` is an exact bound-address match; `0.0.0.0` and `::` are **distinct** wildcards — forbid both to mean "not world-reachable" on dual-stack. `forbid: true` fires when a matching listener exists. |
| `symlinks` | `path`, `target` | The **literal** `readlink` target, never canonicalized: the assertion is about what the link says, not what it resolves to today. |
| `absent` | `path` | ENOENT passes. EPERM is unreadable, not a pass — absence must be provable. |
| `content` | `path`, `contains[]`, `matches[]` | Literal substrings and regexes, each required. Reads cap at 1 MiB; past the cap the check is unreadable-too-large rather than silently truncated. |
| `perms` | `path`, `mode?`, `owner?`, `group?` | Octal mode, exact over `mode & 07777`. Names resolve through `/etc/passwd`/`/etc/group` (a plain parse — NSS/LDAP hosts should assert numeric ids). `lstat` needs no read permission, so this works on secrets the sensor cannot open. |

## Lifecycle

Evaluated every `eval_interval_secs` (hot-swap retimes the loop, and a swap
triggers an immediate sweep). Violations fire alerts (per-expectation
debounce via the reporter); clean rules reconcile; **rules deleted by a
swap resolve their alerts** (seen-rules GC). The `spec` procedure serves the
latest evaluation — `evaluated_at_ms == 0` is the honest "not yet
evaluated", never a fabricated pass.

## Deliberately not in the vocabulary

- **`command`/`run`** — remote execution wearing a monitoring hat. Never.
- **`binary` version checks** — would require executing operator-named
  binaries; #821 decided hostspec executes *nothing*. If toolchain drift
  checking returns, it returns as its own deliberate decision, not a patch.
- **`listening.unit`** (which unit owns the socket) — inode→process→unit
  mapping needs privileged `/proc/<pid>` walks and would be permanently
  "unreadable" for an unprivileged sensor, i.e. a standing false alarm. The
  systemd sentinel's `services_active` answers "is the unit up"; hostspec
  answers "is the socket where it should be".
