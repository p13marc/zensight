# ZenSight Semantic Convention RFC (Draft v0.1)
**Purpose:** Define a Zenoh-native semantic convention and keyspace for ZenSight and potentially other Zenoh applications.

This document is intended for architectural review. It deliberately prioritizes Zenoh's routing, storage, wildcard, ACL, and query capabilities before defining higher-level semantics.

# Goals

- Optimize wildcard subscriptions.
- Support multiple assets, machines, services and protocols.
- Separate routing from ontology.
- Remain evolvable.


# Design philosophy

- Keys are routing addresses, not object graphs.
- Semantic meaning belongs to schemas and catalog.
- Use stable IDs in keys and mutable names in metadata.


# Zenoh capabilities to leverage

- Key expressions (*, **, verbatim chunks).
- Zenoh Storage latest-value model.
- Queryables.
- Liveliness.
- Attachments and encodings.
- ACLs.
- Storage replication.


# Requirements

- Disconnected operation
- Bandwidth constrained links
- Multiple collectors per metric
- Entity correlation
- Historical replay
- Live media
- Commands
- Artifacts


# Survey of existing systems

- Keelson
- Rerun
- OpenTelemetry
- Sparkplug
- OPC UA
- DDS/ROS2


# Canonical terminology

- Realm
- Asset
- Entity
- Producer
- Observer
- Component
- Event
- Relationship
- Recording


# Canonical keyspace

- zensight/v1/<realm>/assets/<asset>/entities/<kind>/<entity>/<state|telemetry>/<domain>/<component>/<producer>
- Separate namespaces for events, catalog, producers, raw, artifacts, queries and @media.


# Wildcard analysis

- Fixed-depth chunks improve subscriptions.
- Avoid $* where practical.
- Reserve verbatim chunks for compatibility boundaries.


# Storage strategy

- State in latest-value storage.
- Telemetry in time-series storage.
- Catalog persisted.
- Media excluded.


# Entity model

- Stable opaque IDs.
- Relationships in catalog.
- Aliases and evidence.


# Commands

- RPC under producers/<id>/rpc/<procedure>.


# Events

- Immutable events with unique IDs.
- Do not model events as mutable state.


# Media

- Dedicated @media plane.


# Artifacts

- Content-addressed descriptors and blobs.


# Schema registry

- Generate Rust constants.
- Map to OpenTelemetry, SNMP, gNMI, Keelson.


# Migration plan

- Prototype registry.
- One vertical slice.
- Migrate sensors progressively.


# Review checklist

- Challenge wildcard efficiency.
- Challenge storage layout.
- Challenge ACLs.
- Suggest alternatives.


# Appendix A - Example keys

```
zensight/v1/mission-a/assets/surface/entities/asset/asset-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/asset/asset-01/telemetry/system/cpu.utilization/sysinfo
```

```
zensight/v1/mission-a/assets/surface/entities/machine/machine-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/machine/machine-01/telemetry/system/cpu.utilization/sysinfo
```

```
zensight/v1/mission-a/assets/surface/entities/service/service-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/service/service-01/telemetry/system/cpu.utilization/sysinfo
```

```
zensight/v1/mission-a/assets/surface/entities/interface/interface-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/interface/interface-01/telemetry/system/cpu.utilization/sysinfo
```

```
zensight/v1/mission-a/assets/surface/entities/process/process-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/process/process-01/telemetry/system/cpu.utilization/sysinfo
```

```
zensight/v1/mission-a/assets/surface/entities/modem/modem-01/state/system/health/self
```

```
zensight/v1/mission-a/assets/surface/entities/modem/modem-01/telemetry/system/cpu.utilization/sysinfo
```


# Appendix B - Subscription patterns

```
assets/surface/**
```

```
entities/machine/*/telemetry/**
```

```
entities/service/*/state/**
```

```
entities/*/*/telemetry/network/*/*
```

```
entities/*/*/telemetry/zenoh/*/*
```

```
events/**
```

```
catalog/**
```

```
producers/*/alive
```


# Appendix C - Questions for reviewers

1. Evaluate design aspect #1 and propose improvements.
2. Evaluate design aspect #2 and propose improvements.
3. Evaluate design aspect #3 and propose improvements.
4. Evaluate design aspect #4 and propose improvements.
5. Evaluate design aspect #5 and propose improvements.
6. Evaluate design aspect #6 and propose improvements.
7. Evaluate design aspect #7 and propose improvements.
8. Evaluate design aspect #8 and propose improvements.
9. Evaluate design aspect #9 and propose improvements.
10. Evaluate design aspect #10 and propose improvements.
11. Evaluate design aspect #11 and propose improvements.
12. Evaluate design aspect #12 and propose improvements.
13. Evaluate design aspect #13 and propose improvements.
14. Evaluate design aspect #14 and propose improvements.
15. Evaluate design aspect #15 and propose improvements.
16. Evaluate design aspect #16 and propose improvements.
17. Evaluate design aspect #17 and propose improvements.
18. Evaluate design aspect #18 and propose improvements.
19. Evaluate design aspect #19 and propose improvements.
20. Evaluate design aspect #20 and propose improvements.
21. Evaluate design aspect #21 and propose improvements.
22. Evaluate design aspect #22 and propose improvements.
23. Evaluate design aspect #23 and propose improvements.
24. Evaluate design aspect #24 and propose improvements.
25. Evaluate design aspect #25 and propose improvements.
26. Evaluate design aspect #26 and propose improvements.
27. Evaluate design aspect #27 and propose improvements.
28. Evaluate design aspect #28 and propose improvements.
29. Evaluate design aspect #29 and propose improvements.
30. Evaluate design aspect #30 and propose improvements.

# Extended rationale

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

A recurring design principle is that Zenoh keys should optimize routing, subscriptions, storage selection, ACL enforcement and replication. The semantic model should remain independent so that entities, relationships, schemas and provenance can evolve without forcing incompatible key changes.

