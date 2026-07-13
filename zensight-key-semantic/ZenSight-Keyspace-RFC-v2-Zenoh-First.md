# ZenSight Keyspace RFC v2 (Zenoh-First)

## Canonical key

```
zensight/@v1/<realm>/assets/<asset>/entities/<kind>/<entity>/<state|telemetry>/<domain>/<component>/<producer>
```

## Top level

```
zensight/@v1/<realm>/assets/<asset>/
  entities/
  producers/
  events/
  catalog/
  queries/
  artifacts/
  raw/
  @media/
```

## Core ideas

- Keys are routing addresses, not the ontology.
- Separate state from telemetry for Zenoh Storage.
- Keep producer in the key to prevent overwrites.
- Keep relationships in the catalog rather than the key hierarchy.
- Raw protocol trees remain under raw/.
- Media is isolated under @media.
- History should be queried through a service instead of backend-specific APIs.

## Storage

State:
```
.../state/**
```

Telemetry:
```
.../telemetry/**
```

Catalog:
```
.../catalog/**
```

Exclude:
```
.../@media/**
```

## Wildcards

Whole asset:
```
assets/surface/**
```

All machine telemetry:
```
entities/machine/*/telemetry/**
```

All Zenoh telemetry:
```
entities/*/*/telemetry/zenoh/*/*
```

## Questions

Review:
1. Wildcard efficiency.
2. Storage compatibility.
3. Replication.
4. ACLs.
5. Producer ownership.
6. Comparison with Keelson.
7. Comparison with Rerun.
8. Better alternatives.
