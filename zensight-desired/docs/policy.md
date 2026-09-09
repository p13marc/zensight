# The fleet policy file

One file decides what every host in the fleet is told to do. It is the file to
review, version and diff; `configs/desired.json5` decides only how to reach the
bus and how often to look.

```json5
{
  classes: [
    { name: "all-hosts",
      matches: { always: true },
      docs: { "sysinfo/thresholds": { rules: [ /* … */ ] } } },

    { name: "hypervisors",
      extends: ["all-hosts"],
      matches: { any: [ { platform: "proxmox-*" } ] },
      docs: { "sysinfo/thresholds": { rules: [ { name: "disk-full", value: 80 } ] } } },
  ],
  hosts: {
    "h-3fa9c2d41b7e": { docs: { "sysinfo/thresholds": { rules: [ { name: "load", value: 64 } ] } } },
  },
}
```

## Selectors

Every selector reads a field of `HostEntity` — the catalog's conclusion, not
anything this daemon decided.

| Selector | Matches |
|---|---|
| `host_id` | exact hashed machine-id |
| `hostname_glob` | glob over hostname or FQDN |
| `sensor` | a sensor that runs on the host (`members[].sensor`) |
| `ip_cidr` | an IP inside the CIDR (v4 and v6) |
| `vendor` | glob over `vendor` |
| `platform` | glob over `platform` |

Combine with `all: [...]` (every selector holds) or `any: [...]` (at least
one). `always: true` is every host in the catalog — spelled out, because an
empty `all: []` reads as "no conditions" and would be a vacuous truth nobody
intended. A class with **no** matcher selects nobody: the safe reading of "I
forgot to say who this is for" is nobody, not everybody.

### `platform` is a glob, and the distinction is load-bearing

Since #935, `platform` is `<ID>-<VERSION_ID>`: `debian-13`, `ubuntu-24.04`,
`proxmox-13`. A class that wants the family writes `debian-*`.

An exact match on `proxmox` selects **nobody**, and nothing says so — the
compiler is not wrong, the class simply has no members. Worse, an exact
`debian-13` silently stops matching after a point-release upgrade. That is the
worst failure a policy compiler has: nothing is broken, no error is raised, the
host just stops receiving configuration.

## The overlay rules

A host's effective document for one topic is:

1. every **matching class**, in file order, each preceded by whatever it
   `extends` (depth-first, so a class always overlays what it builds on);
2. then the classes the **host override** `extends`, expanded the same way —
   `extends` means one thing in this file, so a host asking for `hypervisors`
   gets `all-hosts` too;
3. then the host override's own documents.

A class reached twice — through two chains, or once by matching and once by a
host's `extends` — is applied **once**, at its first position.

Last writer wins a field. Within that:

- **Objects merge recursively** — a class that sets one field does not have to
  restate the rest.
- **`null` deletes.** There is no other way for a later class or a host
  override to *remove* something an earlier one set; without it the only escape
  from an inherited field is not to inherit, which means not using the class.
- **A list of named objects merges by name; any other list replaces.**

### Why lists of named things merge

Replacement is the simpler rule and it was rejected. Every list here is a set
of independent rules, and what an operator wants from a class hierarchy is
"everything the base watches, **plus** these". With replacement, `hypervisors`
adding one expectation silently drops the twelve `all-hosts` contributed — and
the loss is invisible: the document is well-formed, the sensor accepts it, and
twelve conditions stop being watched.

Concatenation was rejected too: a class could then only *add* a rule with the
same name, never change one — and two rules sharing a name share one alert key
(RFC 11 §3.1), the exact collision the sentinels' validators refuse. Merging by
name is the only rule that lets a class both extend and adjust.

The name key is `name`, or `id` for log rules. A list whose items are not all
named objects replaces, because there is nothing to merge on:
`expect_status: [200, 204]` is one value, not a set to accumulate.

## What is refused, and when

`plan`, `apply` and `run` all validate before anything else happens — a policy
that is wrong is wrong for `run` too, and the daemon publishing it unattended
is where the mistake is least visible.

**Refused at load, exit 1:** a duplicate class name (overlay order is file
order, so two classes with one name have no defined precedence); an `extends`
naming an unknown class; an `extends` cycle, reported by naming the loop; a
document key that is not `<producer>/<topic>`; a topic the registry does not
declare; a never-list key in any fragment; and — since #1109 — **a selector that
can never match**.

That last one had been claimed rather than implemented: `ip_in_cidr`'s own doc
comment said "a malformed CIDR is caught by `validate` as a problem, so this
returning `false` is the second line rather than the only one", and `validate`
never looked at a selector, so `false` *was* the only line. `ip_cidr:
"10.0.0.0"` (no prefix), `"10.0.0.0/33"` (impossible for v4) and
`"10.0.0.0/24 "` (a trailing space, and every match here is exact) all parsed,
validated, planned, passed CI and matched zero hosts. The validator reads a CIDR
exactly as `ip_in_cidr` does, so what it accepts is precisely what can match —
a more lenient validator would put the silence back somewhere new.

**Warned by `plan`, not refused:** a class whose selector is well-formed and
simply *wrong* — `platform: "debian"` where the field is `debian-13`, a CIDR for
a subnet that has been renumbered. Nothing about it is malformed, so `validate`
cannot see it; but against a **non-empty** catalog, a class that selects nobody
is worth a line. (Against an empty catalog every class selects nobody and the
observation means nothing, so it is not made.)

**Refused per document, pass continues:** a merged document that does not
deserialize into its registered type. One host's bad override must not stop the
other forty from converging — the alternative, publishing nothing, is the
failure mode with no upper bound on its blast radius. Refusals are logged at
`error`, because a refused document is a host **not** getting the policy
someone wrote, and the sensor will never mention it: nothing reached it.

## Deletion, and the grace

A document is deleted only when the policy stops yielding it for a host the
catalog **still shows** — never because the catalog stopped showing the host —
and then only after `delete_grace_periods` consecutive passes without it.

The two guards do different jobs, and the first is the load-bearing one. A
failed or slow catalog GET produces an **empty fleet**, which is
indistinguishable on the wire from a fleet that really is empty; without the
host check, every document the compiler ever published becomes a deletion
candidate in the same pass, and after `grace × refresh` — ten minutes on the
shipped defaults — the whole fleet reverts to its file baselines. So a key
whose host is not in this pass's catalog is held **indefinitely** and does not
even accrue grace: *"I cannot see it"* is not *"it should have no
configuration"*, and the sensor is reconciling that document quite happily
meanwhile.

The grace then covers the narrower case the first guard lets through: the host
is here, and the policy briefly stopped yielding for it.

`delete_grace_periods: 0` is refused at startup, with that reasoning as the
error.

## What cannot go in a policy

Anything on the `@desired` never-list: secrets, and anything a sensor needs to
**reach the bus** — endpoints, TLS material, the namespace. One bad desired
publish must never lock the fleet out of its own supervision, because the fix
would have to travel over the bus it just broke.

The lint (`zensight_common::desired::never_list_lint`) tests the **value**, not
just the key: `NetlinkExpectations`' `listen` is the TCP port a socket
expectation checks for a listener, and a port is a number. `listen:
"tcp/0.0.0.0:7447"` is refused.

Credentials are referenced by **name** into each host's own config file, never
carried here. That is a property of the payload types, not only of the lint.

## Adoptions: `@rpc/@desired/override/set` (#939)

A per-host exception, recorded durably. This is what turns the GUI's SNMP
discovery from *"copy this JSON5 onto the right host by hand"* into one click:
the proposal becomes an override, the override becomes a document, and the
sensor reconciles it — the workflow this epic exists to delete.

```
GET zensight/v1/@desired/@rpc/override/set?actor=alice
    { "host": "h-3fa9c2d41b7e", "producer": "snmp", "topic": "targets",
      "doc": { "targets": [ … ] }, "note": "adopted from discovery" }
```

`doc: null` **removes** the override — the same deletion idiom the overlay uses
for a field, so there is one rule rather than two. `by` comes from the call's
`?actor=`, never from the body: an author who reports themselves is an author
nobody can be asked about.

Gated by `desired.allow_overrides`, off by default. When off the procedure is
still **served** and replies `error/gated`, so an operator learns the feature
exists and is switched off rather than learning nothing from a timeout.

### It writes a separate file, and that is deliberate

#902 specified this as *"persisted into the policy's `hosts` section"*. That
does not work. `fleet-policy.json5` is hand-written, **commented**, and
hand-ordered — its class order *is* the overlay order — and deserializing it,
mutating `hosts` and re-serializing would strip every comment and normalise the
ordering. The first press of an Adopt button would turn a document an operator
maintains into one a machine emitted.

So adoptions go in `fleet-policy.overrides.json5`, whose entire content the
daemon owns, and where a serde round trip is lossless by construction. What
that buys beyond not destroying anything:

- the reviewable file stays exactly as written, so `git diff` on it means what
  it means;
- what a GUI adopted is visible in one place, separable from what a human
  decided;
- an adoption is reverted by deleting an entry, not by un-editing a merge.

Written atomically — temp file, then rename. A truncating write interrupted
half way would leave a file the next start refuses to parse, which for this
daemon means starting with **no overrides**: silently un-adopting every device
anyone ever added.

### Where they overlay

**Last** — after every class, and after the policy's own `hosts` section.
Someone pressed a button while looking at that host; that is the most specific
statement there is. `render <host>` shows the result, so the merged view is
still one command away.

A host that matches no class still receives its adoption. Otherwise adopting a
device on a machine the policy says nothing about — exactly the discovery
case — would silently do nothing.
