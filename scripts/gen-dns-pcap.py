#!/usr/bin/env python3
"""Generate the committed pcap fixture for the netring passive-DNS +
FQDN-beacon integration test (issue #308).

    tests/fixtures/passive_dns.pcap  (zensight-sensor-netring)

Regeneration only — the generated fixture is checked in, so this script never
runs at build or test time. Pure stdlib (no scapy), fully deterministic: fixed
base timestamp, fixed addresses/ports/payloads.

Traffic story (client 10.0.0.5, resolver 10.0.0.8):

1. A query `beacon.evil.example` -> response with a CNAME chain
   (`beacon.evil.example` CNAME `cdn.evil.example`) terminating in
   A 93.184.216.34 (TTL 86400) — the flow/talker name-enrichment source.
2. A PTR exchange for 10.0.0.9 -> `ptr.evil.example` — reverse-claim coverage.
3. 16 identical-size TCP flows 10.0.0.5:<ephemeral> -> 93.184.216.34:443,
   spaced exactly 30 s — periodic, size-constant "C2" that drives the
   FQDN-keyed RITA beacon detector over its threshold.

Usage: python3 scripts/gen-dns-pcap.py
"""

import struct
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / (
    "zensight-sensor-netring/tests/fixtures/passive_dns.pcap"
)

BASE = 1_700_000_000  # fixed epoch base: deterministic output
CLIENT = "10.0.0.5"
RESOLVER = "10.0.0.8"
BEACON_DST = "93.184.216.34"
PTR_IP = "10.0.0.9"
TTL = 86_400  # answers outlive the whole capture window

CLIENT_MAC = bytes.fromhex("aabbcc000005")
OTHER_MAC = bytes.fromhex("aabbcc000008")


def ip4(addr: str) -> bytes:
    return bytes(int(o) for o in addr.split("."))


def checksum(data: bytes) -> int:
    if len(data) % 2:
        data += b"\x00"
    s = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ether(src: bytes, dst: bytes, payload: bytes) -> bytes:
    return dst + src + struct.pack("!H", 0x0800) + payload


def ipv4(proto: int, src: str, dst: str, payload: bytes, ident: int) -> bytes:
    total = 20 + len(payload)
    hdr = struct.pack(
        "!BBHHHBBH4s4s", 0x45, 0, total, ident, 0x4000, 64, proto, 0, ip4(src), ip4(dst)
    )
    hdr = hdr[:10] + struct.pack("!H", checksum(hdr)) + hdr[12:]
    return hdr + payload


def l4_checksum(proto: int, src: str, dst: str, seg: bytes) -> int:
    pseudo = ip4(src) + ip4(dst) + struct.pack("!BBH", 0, proto, len(seg))
    return checksum(pseudo + seg) or 0xFFFF


def udp(src: str, dst: str, sport: int, dport: int, payload: bytes) -> bytes:
    seg = struct.pack("!HHHH", sport, dport, 8 + len(payload), 0) + payload
    ck = l4_checksum(17, src, dst, seg)
    return seg[:6] + struct.pack("!H", ck) + seg[8:]


def tcp(src: str, dst: str, sport: int, dport: int, seq: int, payload: bytes) -> bytes:
    # PSH+ACK data segment (flow tracking does not require a handshake).
    seg = (
        struct.pack("!HHIIBBHHH", sport, dport, seq, 1, 0x50, 0x18, 64_240, 0, 0)
        + payload
    )
    ck = l4_checksum(6, src, dst, seg)
    return seg[:16] + struct.pack("!H", ck) + seg[18:]


def dns_name(name: str) -> bytes:
    out = b""
    for label in name.rstrip(".").split("."):
        raw = label.encode()
        out += bytes([len(raw)]) + raw
    return out + b"\x00"


def dns_query(txid: int, qname: str, qtype: int) -> bytes:
    return (
        struct.pack("!HHHHHH", txid, 0x0100, 1, 0, 0, 0)
        + dns_name(qname)
        + struct.pack("!HH", qtype, 1)
    )


def dns_answer(name: str, rtype: int, rdata: bytes) -> bytes:
    return dns_name(name) + struct.pack("!HHIH", rtype, 1, TTL, len(rdata)) + rdata


def dns_response(txid: int, qname: str, qtype: int, answers: list[bytes]) -> bytes:
    return (
        struct.pack("!HHHHHH", txid, 0x8180, 1, len(answers), 0, 0)
        + dns_name(qname)
        + struct.pack("!HH", qtype, 1)
        + b"".join(answers)
    )


packets: list[tuple[float, bytes]] = []
ident = 0


def add(ts: float, frame: bytes) -> None:
    packets.append((ts, frame))


def add_udp(ts, src, dst, smac, dmac, sport, dport, payload):
    global ident
    ident += 1
    add(ts, ether(smac, dmac, ipv4(17, src, dst, udp(src, dst, sport, dport, payload), ident)))


def add_tcp(ts, src, dst, smac, dmac, sport, dport, seq, payload):
    global ident
    ident += 1
    add(ts, ether(smac, dmac, ipv4(6, src, dst, tcp(src, dst, sport, dport, seq, payload), ident)))


# 1. Forward lookup with a CNAME chain (client -> resolver -> client).
add_udp(
    BASE + 0.000, CLIENT, RESOLVER, CLIENT_MAC, OTHER_MAC, 53_444, 53,
    dns_query(0x1234, "beacon.evil.example", 1),
)
add_udp(
    BASE + 0.050, RESOLVER, CLIENT, OTHER_MAC, CLIENT_MAC, 53, 53_444,
    dns_response(
        0x1234, "beacon.evil.example", 1,
        [
            dns_answer("beacon.evil.example", 5, dns_name("cdn.evil.example")),
            dns_answer("cdn.evil.example", 1, ip4(BEACON_DST)),
        ],
    ),
)

# 2. Reverse (PTR) lookup for another IP.
PTR_QNAME = ".".join(reversed(PTR_IP.split("."))) + ".in-addr.arpa"
add_udp(
    BASE + 0.100, CLIENT, RESOLVER, CLIENT_MAC, OTHER_MAC, 53_445, 53,
    dns_query(0x1235, PTR_QNAME, 12),
)
add_udp(
    BASE + 0.150, RESOLVER, CLIENT, OTHER_MAC, CLIENT_MAC, 53, 53_445,
    dns_response(0x1235, PTR_QNAME, 12, [dns_answer(PTR_QNAME, 12, dns_name("ptr.evil.example"))]),
)

# 3. The beacon: 16 identical-size TCP data segments, exactly 30 s apart, each
# on a fresh ephemeral port (fresh flow) to the resolved beacon destination.
for i in range(16):
    add_tcp(
        BASE + 10 + 30 * i, CLIENT, BEACON_DST, CLIENT_MAC, OTHER_MAC,
        40_000 + i, 443, 1000 + i, b"A" * 120,
    )

# Write classic pcap: little-endian magic, linktype 1 (Ethernet).
with OUT.open("wb") as f:
    f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65_535, 1))
    for ts, frame in packets:
        sec = int(ts)
        usec = round((ts - sec) * 1_000_000)
        f.write(struct.pack("<IIII", sec, usec, len(frame), len(frame)))
        f.write(frame)

print(f"wrote {OUT} ({len(packets)} packets)")
