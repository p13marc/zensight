# Reliable log delivery: RELP vs TCP+TLS + sender queueing (#551)

**Status:** decision doc (research, p2). **Recommendation: document sender-side
disk-assisted queueing over TCP/TLS as the supported reliable path; defer a RELP
listener** until a concrete deployment needs it. No new protocol code lands now.

## The problem

Syslog transports differ in what they guarantee on failure:

| Transport | On connection abort / receiver restart |
|---|---|
| UDP (RFC 5426) | datagrams silently lost; no delivery signal at all |
| TCP (RFC 6587) | bytes in the sender's socket buffer + in flight are lost; the sender learns the socket closed but not *which* messages the receiver processed |
| TLS (RFC 5425) | same as TCP — TLS secures the bytes, it does not acknowledge application-level receipt |
| RELP | per-message command/response acks + windowing: the sender only retires a message once the receiver has acked it, so an abort re-sends unacked messages |

So even after #550 (TLS), a receiver restart or mid-stream abort can drop the
handful of messages that were in flight but not yet processed. RELP closes that
gap with an application-level ack backchannel.

## Research findings

### 1. What do our senders actually support?

- **rsyslog** — RELP native (`omrelp`/`imrelp`), and also disk-assisted queues
  that survive a receiver outage over plain TCP/TLS.
- **syslog-ng** — **no RELP.** It offers its own `RLTP`/flow-control and
  disk-buffering, but nothing wire-compatible with a RELP listener.
- **Network appliances / embedded** (switches, firewalls, IoT) — almost always
  plain UDP or TCP only; neither RELP nor durable queues.

The reliable-delivery-capable population is essentially *rsyslog*. And rsyslog
already has a reliability story over the transports we support (disk-assisted
queue + TCP/TLS) that does not require us to implement anything.

### 2. How much is actually at risk?

The residual loss RELP would prevent is bounded to **messages in flight during a
connection abort** — not a steady-state leak:

- Local host logs are covered by **journald** (#57), which has its own durable
  cursor and is unaffected by network transport.
- At-rest durability on the receiver is covered by the ring + the durable store
  (#544); ingest loss under load is bounded and *counted* (#546:
  `ingest/dropped_ratio`, sustained-loss alert), so transport loss is
  observable rather than silent.
- The exposure is therefore: a network sender's in-flight window (typically a
  few messages) at the moment a connection drops, for senders that are **not**
  journald-local. For an rsyslog sender with a disk-assisted queue, even that
  window is retried by the sender.

A harness experiment (kill a TCP connection mid-stream and count delivered vs
sent) confirms the shape: plain TCP loses only the unacked tail at the abort
point, not a proportion of the stream. The loss is real but small and episodic,
and it overlaps heavily with cases the sender or journald already covers.

### 3. Implementation cost of a RELP listener

- **No maintained Rust RELP crate exists.** A listener would be a from-scratch
  state machine: the `open`/`syslog`/`close` command framing, per-command
  response codes, the ack-window bookkeeping, and a TLS variant. It also needs
  its own conformance tests against rsyslog's `omrelp` as the reference sender.
- That is a meaningful, ongoing maintenance surface (a new wire protocol with
  its own security review) for an audience of rsyslog-only senders that already
  have a supported reliable path.

## Decision

**Document sender-side disk-assisted queueing over TCP/TLS as the supported
reliable-delivery configuration; defer implementing a RELP listener.**

Rationale: the capable sender population is rsyslog, which already achieves
end-to-end reliability with a disk-assisted queue + our TCP/TLS listener; the
residual RELP-only benefit is a small, observable, episodic in-flight window
that mostly overlaps with journald/sender coverage; and a RELP listener is a
from-scratch protocol + security surface with no library to lean on. The cost
is not justified by the marginal reliability gain **for our fleet today**.

### Supported reliable configuration (operator guidance)

Point reliability-sensitive senders at the **TLS** listener (#550) and enable a
disk-assisted queue on the sender so an outage is retried, not lost. rsyslog:

```rsyslog
# rsyslog omfwd → zensight TLS listener, with a disk-assisted queue.
$DefaultNetstreamDriver ossl
action(
  type="omfwd" target="logs.example.com" port="6514"
  protocol="tcp" StreamDriver="ossl" StreamDriverMode="1"
  StreamDriverAuthMode="x509/name"
  queue.type="LinkedList" queue.filename="fwd-zensight"
  queue.saveOnShutdown="on" action.resumeRetryCount="-1"
)
```

This survives a receiver restart (the sender re-connects and drains its queue),
which is the practical equivalent of RELP's guarantee for the transports we
support — with no new protocol on our side.

## Revisit criteria — when to implement RELP

File a scoped follow-up (with these as the trigger) if any becomes true:

- A required deployment's reliable senders are RELP-only (cannot disk-queue over
  TCP/TLS), **or**
- Measured in-flight loss at a real deployment is operationally significant
  (not covered by journald/sender queues), **or**
- A maintained, audited Rust RELP crate appears, collapsing the implementation
  and security cost.

Until then this is **deferred**, not rejected — the octet-counting `FrameReader`
(#546) and the TLS transport (#550) are exactly the substrate a future RELP
listener would build on, so the deferral carries no rework debt.
