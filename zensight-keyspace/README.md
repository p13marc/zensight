# zensight-keyspace

The executable form of the **keyspace-v2 convention**
([`docs/rfcs/keyspace-v2/`](../docs/rfcs/keyspace-v2/00-index.md), v1).
Producers and consumers emit and parse conforming keys through this crate and
never spell raw key strings.

```text
@v1/<origin>/<class>/<producer>/<subject...>        (base-relative; the base
                                                     is the session namespace)
```

| Module | Enforces | Mechanism |
|---|---|---|
| `grammar` | RFC 03 — chunk charset, reserved tokens, structural assembly/parse | validation + typed builders: an invalid key does not construct |
| `origin` | RFC 06 §1 — `h-<12hex>` minting, fallbacks | one function, pinned to the RFC test vector |
| `slug` | RFC 03 §2 — RFC-5952 IP canon, lossless `_xNN_` escape | pure functions, injectivity-tested |
| `qos` | RFC 04 §3 — the five named profiles | closed enum → zenoh QoS triple |
| `tests/guard.rs` | RFC 03 §4 — design properties D1–D6, ACL inclusion | key algebra pinned as CI tests |

The subject vocabulary is governed by the registry (RFC 08); generated
per-subject builders/parsers are produced by this crate's build script from
`registry/*.toml` (epic #453, issue #455).

Migration status: introduced by epic
[#453](https://github.com/p13marc/zensight/issues/453); absorbs
`zensight-common/src/{keyexpr,command}.rs` as the waves land.
