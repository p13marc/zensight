// The control plane against a REAL bus: bridge + parallax sensor with its
// synthetic test source, no pixels. Not a unit test — it needs the topology
// `just web-smoke` stands up — but it is the acceptance of #706: origin
// resolved from liveliness, catalogue read, a tier opened and seen open in
// the status document, a keyframe requested, the tier closed with a
// profile-correct close and seen closed.
//
//   ZENSIGHT_BRIDGE=ws://localhost:10000 npm run smoke
//
// Runs under Node with `--experimental-websocket --experimental-wasm-modules`
// (zenoh-ts wants a global WebSocket and imports its key-expression checker
// as an ESM wasm module); see the `smoke` script in package.json.
import { connect } from "../src/bus.js";
import { fetchCatalogue, tierChoices, watchStatus } from "../src/catalogue.js";
import { ControlPlane, Profile, type Sent } from "../src/control.js";
import { listOrigins } from "../src/origins.js";
import { describe } from "../src/rpc.js";
import type { StreamStatus } from "../src/types.gen.js";

const locator = process.env["ZENSIGHT_BRIDGE"] ?? "ws://localhost:10000";
const fail = (msg: string): never => {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
};

const bus = await connect(locator);
console.log(`connected to ${locator}`);

const origins = await listOrigins(bus);
console.log(`parallax origins alive: ${origins.map((o) => o.value).join(", ") || "none"}`);
const origin = origins[0] ?? fail("no parallax sensor is alive — is the sensor connected to the bridge's hub?");

const catalogue = (await fetchCatalogue(bus, origin)) ?? fail("the catalogue did not answer");
console.log(`catalogue: ${catalogue.map((d) => `${d.stream} [${d.codecs.join("/")}] tiers ${tierChoices(d).join(",")}`).join("; ")}`);
const stream = catalogue.find((d) => d.codecs.includes("h264") && tierChoices(d).length > 0)
  ?? fail("no stream offers h264 with a tier ladder");
const tier = tierChoices(stream)[0]!;

let latest: ReadonlyMap<string, StreamStatus> = new Map();
const waiters: Array<() => void> = [];
const stop = await watchStatus(bus, origin, (m) => {
  latest = m;
  for (const w of waiters.splice(0)) w();
});
const until = async (what: string, pred: () => boolean, ms = 8000): Promise<void> => {
  const deadline = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > deadline) fail(`timed out waiting for ${what}; last status: ${JSON.stringify([...latest.values()])}`);
    await new Promise<void>((r) => {
      waiters.push(r);
      setTimeout(r, 200);
    });
  }
};

const sent: Sent[] = [];
const cp = new ControlPlane(bus, origin, (s) => {
  sent.push(s);
  console.log(`  ${JSON.stringify(s.command)} → ${describe(s.outcome)}`);
});
const profile = Profile.video(stream.stream, tier);

console.log(`open ${stream.stream}/h264/${tier}`);
const opened = await cp.open(profile);
if (!opened.ok) fail(`open refused: ${describe(opened)}`);
await until("the status to say open", () => latest.get(stream.stream)?.open === true);
console.log(`  status: ${JSON.stringify(latest.get(stream.stream))}`);

const kf = await cp.requestKeyframe(profile);
if (!kf.ok) fail(`keyframe refused: ${describe(kf)}`);

console.log(`close ${stream.stream}/h264/${tier} (profile-correct)`);
const closed = await cp.close(profile);
if (!closed.ok) fail(`close refused: ${describe(closed)}`);
// A close decrements the profile's refcount; at zero the profile enters the
// sensor's idle countdown (`idle_timeout_secs`, 30 s in configs/parallax.json5)
// and is reaped — and reported closed — when it expires with no subscriber on
// the media key. So "closed" is not immediate, by contract: the subscriber's
// falling edge (#707) is the other input, and a refcount alone does not tear
// a pipeline down while a viewer may still be on it.
console.log("  waiting for the idle countdown to reap the tier (idle_timeout_secs) …");
await until("the status to say closed after the idle window", () => latest.get(stream.stream)?.open === false, 45_000);
console.log(`  status: ${JSON.stringify(latest.get(stream.stream))}`);

const last = sent.at(-1)?.command;
if (last?.type !== "close_stream" || last.codec !== "h264" || last.tier !== tier) fail("the close did not name its profile");

await stop();
await bus.close();
console.log("OK: origin resolved, catalogue read, tier opened and seen open, keyframe asked, closed profile-correctly and seen closed");
process.exit(0);
