# zensight-sensor-hostspec

Machine-checked desired-state assertions (#821): the sentinel pattern
(netlink #278 / systemd #277) for the things D-Bus and netlink cannot see —
mounts, files, listening sockets, symlinks, absence, file content,
permissions. Reality drifting from the ops document is the failure class
this sensor exists for; every audit finding of the reference fleet had that
shape, found by a human weeks late.

- **Closed vocabulary, read-only, executes nothing.** There is no
  command/run assertion and there never will be (a remote-execution surface
  wearing a monitoring hat), and no binary/version assertion either (#821
  design decision): the whole I/O surface is `/proc/self/mountinfo`,
  `/proc/net/tcp{,6}`, `lstat`/`readlink`, bounded file reads and
  `/etc/passwd`+`group`. No capabilities of any kind — the least privileged
  sensor in the fleet.
- **Failures are alerts** on `state/hostspec/alert/*`, the failing clause in
  the labels (`check`/`path`/`expected`/`actual`), per-expectation severity
  and debounce. One gauge, `telemetry/hostspec/assertions/failing`,
  publishes the failing count every sweep — 0 included, so an empty set
  reads as 0, never as silence.
- **Hot-swappable** over `@rpc/hostspec/expectations/set` (whole-set replace,
  validated before apply — a bad regex refuses with `error/invalid-args` and
  the previous good set keeps running); **answerable** on
  `@rpc/hostspec/spec`: per-assertion pass / fail / **unreadable** — an
  observation the sensor could not make is never a pass.

Docs: [`docs/assertions.md`](docs/assertions.md) (the vocabulary and its
semantics), [`docs/configuration.md`](docs/configuration.md). The shipped
[`configs/hostspec.json5`](../configs/hostspec.json5) is the operator
reference — its default set is empty on purpose.

**The honest Ansible overlap:** this looks like configuration management's
job and is not. Ansible converges the machine at run time; hostspec notices
drift *between* converges — and it is the thing that tells you the converge
never ran. If an IaC track lands, generate the expectation set from the same
inventory rather than authoring it twice.
