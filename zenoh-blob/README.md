# zenoh-blob

Generic, resumable, chunked **blob and directory transfer over [Zenoh]** — with
progress, SHA-256 integrity, range resume that survives reconnect *and* restart,
and bounded memory. No application-specific types; it's the large-payload path
the Zenoh ecosystem is otherwise missing.

[Zenoh]: https://zenoh.io

## Why

Zenoh is excellent at pub/sub and query, but has no turnkey way to pull a large
artifact (a debug bundle, a pcap, a dataset, a directory tree) from one peer to
another with progress and resume. `zenoh-blob` builds that on the primitives
Zenoh already gives you — multi-reply queryables, a reliable transport, and
`CongestionControl::Block` backpressure — so you don't fork a file-sync tool to
get it.

## Two tiers

**Tier 1 — single blob.** One queryable serves every blob under a key prefix.
A download is two queries: fetch the [`Manifest`] (so the client learns
`chunk_size` and the whole-blob hash), then stream the chunks. Zenoh does **not**
order query replies, so the client places each (out-of-order) chunk at its byte
offset and writes straight to disk — memory stays `O(chunk_size)` regardless of
blob size. A `.part` file + sidecar bitmap make resume a `?from=K` re-query; the
final whole-blob SHA-256 is verified before the file is renamed into place.

```rust
// Server
let server = zenoh_blob::BlobServer::new(session.clone(), "demo/blobs", Format::Json);
server.register(manifest, Arc::new(zenoh_blob::FileBlobSource::new(&path))).await;
tokio::spawn(server.run());

// Client
let client = zenoh_blob::BlobClient::new(session, "demo/blobs", Format::Json);
let path = client.download("blob-1", &dest_dir, &()).await?;
```

**Tier 2 — content-addressed directories** (the [casync] model). A snapshot is a
[`TreeIndex`] (a depth-first entry list; files reference their chunks by content
hash) plus a content-addressed chunk store. The client fetches only the chunks it
is **missing** (`needed − have`), re-hashing each on receipt, and reconstructs
the tree. Progress *is* "which hashes are on disk", so an interrupted pull
resumes for free and identical chunks (across files or versions) transfer once.
[FastCDC] content-defined chunking localizes edits so a small change re-transfers
only its neighborhood. Snapshots can be served live ([`TreeServer`]) or published
into a Zenoh storage so the producer can exit.

[casync]: https://github.com/systemd/casync
[FastCDC]: https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia

```rust
let (index, chunks) = zenoh_blob::build_tree(dir, "snap-1", &chunker)?;
// serve live...
let server = zenoh_blob::TreeServer::new(session, "demo/store", "demo/tree", Format::Cbor, store);
server.register(index).await;
// ...or publish into a router storage and exit:
zenoh_blob::publish_snapshot(&session, "demo/store", "demo/tree", &index, chunks, Format::Cbor).await?;

// client
let client = zenoh_blob::TreeClient::new(session, "demo/store", "demo/tree", Format::Cbor);
client.download_tree("snap-1", &dest, &content_store).await?;
```

## Design notes

- **Backpressure is automatic.** `Session::get` defaults to
  `CongestionControl::Block` and replies inherit it, so chunk replies block rather
  than drop under load. The crate sets **no** congestion control explicitly (the
  setter is behind Zenoh's `internal` feature, deliberately not enabled).
- **Reply keys must match the query.** Clients GET the `<prefix>/<id>/**` wildcard
  so the `chunk/<i>` replies are accepted (`ReplyKeyExpr::MatchingQuery`).
- **Pluggable** hashing (`Digest`), chunking (`Chunker`: fixed-size or FastCDC),
  and encoding (`Format`: JSON or CBOR).

## Acknowledgements

The design borrows ideas from prior art in the space: [casync] (content-addressed
trees + chunk stores), [desync], [`zenoh-fs`] and [sendit] (file transfer over
Zenoh), and the [FastCDC] paper (content-defined chunking). `zenoh-blob` is an
independent implementation, not a fork of any of them.

[desync]: https://github.com/folbricht/desync
[`zenoh-fs`]: https://github.com/atolab/zenoh-fs
[sendit]: https://github.com/eclipse-zenoh/zenoh-demos

## License

Licensed under the [MIT license](LICENSE).
