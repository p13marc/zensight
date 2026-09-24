// The key spellings the browser needs, transliterated from
// `zensight_common::keyexpr` and the parallax registry (RFC 03 / RFC 07).
//
// Keys are BASE-LESS: the deployment namespace, if any, is set on the
// remote-api bridge (`configs/router-remote-api.json5`) and never here — the
// plugin creates every browser session from the bridge's runtime, whose
// namespace applies to everything this page spells (#705).

/** RFC 03 §1.3: a host origin is `h-` + 12 lowercase hex, exactly. */
export const HOST_ORIGIN = /^h-[0-9a-f]{12}$/;

export const PRODUCER = "parallax";

/**
 * A validated host origin. The `*` origin is unrepresentable on purpose:
 * RFC 07 §3 forbids wildcarding the origin on `@media` — every matching host
 * would ship the full payload — and this client applies the same rule to the
 * control plane, because a stream control is a statement to ONE host's
 * sensor and the fleet spelling would open, close or retune the stream on
 * every host serving parallax (the iced GUI's rule since #1261).
 */
export class Origin {
  private constructor(readonly value: string) {}

  static parse(s: string): Origin | undefined {
    const v = s.trim();
    return HOST_ORIGIN.test(v) ? new Origin(v) : undefined;
  }

  /** Throws on a malformed origin; for callers that already validated. */
  static of(s: string): Origin {
    const o = Origin.parse(s);
    if (!o) throw new Error(`not a host origin: ${JSON.stringify(s)} (want h-<12 hex>)`);
    return o;
  }

  toString(): string {
    return this.value;
  }
}

/** `v1/<origin>/@rpc/parallax/streams` — the catalogue read procedure (JSON `Vec<StreamDescriptor>`). */
export function streamsKey(origin: Origin): string {
  return `v1/${origin.value}/@rpc/${PRODUCER}/streams`;
}

/** `v1/<origin>/@rpc/parallax/stream/set` — the write procedure taking `Command<StreamControl>`. */
export function streamSetKey(origin: Origin): string {
  return `v1/${origin.value}/@rpc/${PRODUCER}/stream/set`;
}

/**
 * `v1/<origin>/state/parallax/stream/<stream>` — one stream's `StreamStatus`
 * document (JSON), or the `*` selector for all of a host's streams.
 */
export function streamStatusKey(origin: Origin, stream: string | "*" = "*"): string {
  return `v1/${origin.value}/state/${PRODUCER}/stream/${stream}`;
}

/**
 * The liveliness token every parallax sensor declares
 * (`<origin>/state/parallax/alive`, RFC 04 §5), as the fleet-wide selector a
 * browser resolves origins from. A `*` in the ORIGIN position is legal here:
 * a token is not a media payload, and `v1/*` never matches a verbatim `@`
 * service origin (design property D4), which is exactly right for a producer
 * that only ever runs on hosts.
 */
export function aliveSelector(): string {
  return `v1/*/state/${PRODUCER}/alive`;
}

/** The origin a liveliness sample's key belongs to, or `undefined` if it is not a parallax token. */
export function originOfAliveKey(key: string): Origin | undefined {
  const m = /^v1\/(h-[0-9a-f]{12})\/state\/parallax\/alive$/.exec(key);
  return m?.[1] ? Origin.parse(m[1]) : undefined;
}

/** The stream a status sample's key names, or `undefined` if it is not one. */
export function streamOfStatusKey(key: string, origin: Origin): string | undefined {
  const prefix = `v1/${origin.value}/state/${PRODUCER}/stream/`;
  if (!key.startsWith(prefix)) return undefined;
  const rest = key.slice(prefix.length);
  return rest.length > 0 && !rest.includes("/") ? rest : undefined;
}

/**
 * `v1/<origin>/@media/parallax/<stream>/video/<codec>/<tier>` — EXACT tier
 * key. RFC 07 §3 revoked the `…/video/h264/*` licence: every tier is
 * published concurrently on its own key, so a wildcard would deliver several
 * interleaved H.264 streams on one subscriber. The subscription is the
 * quality choice (#707 subscribes here; this module only spells it).
 */
export function mediaVideoKey(origin: Origin, stream: string, codec: string, tier: string): string {
  return `v1/${origin.value}/@media/${PRODUCER}/${stream}/video/${codec}/${tier}`;
}

/** `v1/<origin>/@media/parallax/<stream>/preview/jpeg` — the JPEG preview. */
export function mediaPreviewKey(origin: Origin, stream: string): string {
  return `v1/${origin.value}/@media/${PRODUCER}/${stream}/preview/jpeg`;
}
