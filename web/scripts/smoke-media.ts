// The media plane through the bridge, without a browser (#707): subscribe to
// one EXACT tier key, prove that samples arrive with a CBOR FrameMeta
// attachment and an HLC timestamp intact, that the first admitted frame is a
// keyframe whose SPS yields a WebCodecs codec string, that sequences are
// contiguous, and that a receiver report is accepted by the sensor. No
// decoding — WebCodecs is the browser's; everything the tile does before the
// decoder is exercised here against the real sensor.
//
//   ZENSIGHT_BRIDGE=ws://localhost:10000 npm run smoke:media
import { connect } from "../src/bus.js";
import { fetchCatalogue, tierChoices } from "../src/catalogue.js";
import { ControlPlane, Profile } from "../src/control.js";
import { Origin, mediaVideoKey } from "../src/keys.js";
import { listOrigins } from "../src/origins.js";
import { Reporter } from "../src/report.js";
import { describe } from "../src/rpc.js";
import { VideoTile, type Chunk, type Decoder } from "../src/tile.js";

const locator = process.env["ZENSIGHT_BRIDGE"] ?? "ws://localhost:10000";
const fail = (msg: string): never => {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
};

const bus = await connect(locator);
const origin: Origin = (await listOrigins(bus))[0] ?? fail("no parallax sensor is alive");
const catalogue = (await fetchCatalogue(bus, origin)) ?? fail("no catalogue");
const stream = catalogue.find((d) => d.codecs.includes("h264") && tierChoices(d).length > 0) ?? fail("no h264 stream");
const tier = tierChoices(stream)[0]!;
const profile = Profile.video(stream.stream, tier);
const cp = new ControlPlane(bus, origin, (s) => console.log(`  ${JSON.stringify(s.command)} → ${describe(s.outcome)}`));

/** Counts what the tile would have handed WebCodecs. */
class CountingDecoder implements Decoder {
  codec: string | undefined;
  chunks: Chunk[] = [];
  configure(codec: string): void {
    this.codec = codec;
  }
  decode(chunk: Chunk): void {
    this.chunks.push(chunk);
  }
  reset(): void {}
  queueSize(): number {
    return 0;
  }
  close(): void {}
}
const decoder = new CountingDecoder();
let keyframeRequests = 0;
let reportsSent = 0;
let stamped = 0;
let withAttachment = 0;
let samples = 0;
const reporter = new Reporter(bus, origin, (line) => console.log(`  ${line}`));
const tile = new VideoTile({
  stream: stream.stream,
  tier,
  decoder,
  maxLiveLatencyMs: 1500,
  events: {
    requestKeyframe: () => {
      keyframeRequests++;
      void cp.requestKeyframe(profile);
    },
    report: (r) => {
      reportsSent++;
      reporter.send(r);
      console.log(`  report: received ${r.received_frames} lost ${r.lost_frames} dropped ${r.dropped_frames} age ${r.frame_age_ms?.toFixed(1) ?? "n/a"} ms`);
    },
    ended: (reason) => console.log(`  tile ended: ${reason ?? "closed"}`),
    configured: (codec) => console.log(`  decoder configured: ${codec}`),
  },
});

const key = mediaVideoKey(origin, stream.stream, "h264", tier);
console.log(`subscribe ${key}`);
const undeclare = await bus.subscribe(key, (s) => {
  samples++;
  if (s.attachment) withAttachment++;
  if (s.publishedMs !== undefined) stamped++;
  tile.onSample({ payload: s.payload, attachment: s.attachment, publishedMs: s.publishedMs });
  // Pretend the decoder produced every chunk it was given, so the gate clears healthily.
  const last = decoder.chunks.at(-1);
  if (last && s.attachment) tile.onDecoded({ sequence: last.sequence, keyframe: last.type === "key" });
});
console.log(`open ${stream.stream}/h264/${tier} — the Nth-viewer rule: we also ask for a keyframe`);
const opened = await cp.open(profile);
if (!opened.ok) fail(`open refused: ${describe(opened)}`);
await cp.requestKeyframe(profile);

await new Promise((r) => setTimeout(r, 7000));

console.log(`close ${stream.stream}/h264/${tier} and undeclare (the falling edge)`);
tile.close();
await undeclare();
await cp.close(profile);
await bus.close();

console.log(`samples ${samples}, with attachment ${withAttachment}, stamped ${stamped}, decoded-through ${decoder.chunks.length}, keyframe asks ${keyframeRequests}, reports ${reportsSent}`);
if (samples === 0) fail("no sample arrived on the tier key through the bridge");
if (withAttachment !== samples) fail("a sample crossed the bridge without its FrameMeta attachment");
if (stamped !== samples) fail("a sample crossed the bridge without its HLC timestamp");
if (decoder.codec === undefined) fail("no keyframe with an SPS was admitted — no codec string");
if (decoder.chunks[0]?.type !== "key") fail("the first chunk handed to the decoder was not a keyframe");
for (let i = 1; i < decoder.chunks.length; i++) {
  if (decoder.chunks[i]!.sequence !== decoder.chunks[i - 1]!.sequence + 1n) fail(`sequence gap between admitted chunks at ${i}`);
}
const counts = tile.stats.counts();
if (counts.lost !== 0n) fail(`lost ${counts.lost} frame(s) on loopback`);
if (reportsSent < 2) fail("fewer than two reports in seven seconds");
console.log(`OK: ${decoder.chunks.length} access units admitted in order from a keyframe (${decoder.codec}), attachments and timestamps intact, ${reportsSent} reports accepted`);
process.exit(0);
