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
declare; a never-list key in any fragment.

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
