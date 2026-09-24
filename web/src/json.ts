// JSON on the wire, dependency-free so the client modules stay importable
// without zenoh-ts's wasm (the unit tests run against a fake bus).
//
// A sensor answers the control channel in JSON: `decode_auto` sniffs the
// request (`{`/`[` ⇒ JSON), the `streams` reply is `serde_json`, and the
// stream status documents are `publish_json`. Only the media attachment
// (#707) is strictly CBOR.
const utf8 = new TextDecoder();

export function json<T>(bytes: Uint8Array): T {
  return JSON.parse(utf8.decode(bytes)) as T;
}
