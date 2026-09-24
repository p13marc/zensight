import { describe, expect, it } from "vitest";
import { FakeBus } from "./fake.js";
import { listOrigins, watchOrigins, type OriginsView } from "./origins.js";

describe("origin resolution (trap 2)", () => {
  it("lists the hosts whose parallax token is up, sorted, from the fleet selector", async () => {
    const bus = new FakeBus();
    bus.alive = [
      "v1/h-ffffffffffff/state/parallax/alive",
      "v1/h-3fa9c2d41b7e/state/parallax/alive",
      "v1/h-3fa9c2d41b7e/state/sysinfo/alive", // another producer: not an origin here
      "v1/h-3fa9c2d41b7e/state/parallax/device/cam0/alive", // a device token: not an origin
    ];
    const origins = await listOrigins(bus);
    expect(origins.map((o) => o.value)).toEqual(["h-3fa9c2d41b7e", "h-ffffffffffff"]);
  });

  it("follows edges: replay, a host joining, a host leaving", async () => {
    const bus = new FakeBus();
    bus.alive = ["v1/h-3fa9c2d41b7e/state/parallax/alive"];
    const views: OriginsView[] = [];
    const undeclare = await watchOrigins(bus, (v) => views.push(v));
    expect(bus.livelinessSubs.has("v1/*/state/parallax/alive")).toBe(true);
    // replay + the explicit emit after subscribing
    expect(views.at(-1)?.alive.map((o) => o.value)).toEqual(["h-3fa9c2d41b7e"]);
    bus.token("v1/h-000000000000/state/parallax/alive", true);
    expect(views.at(-1)?.alive.map((o) => o.value)).toEqual(["h-000000000000", "h-3fa9c2d41b7e"]);
    bus.token("v1/h-3fa9c2d41b7e/state/parallax/alive", false);
    expect(views.at(-1)?.alive.map((o) => o.value)).toEqual(["h-000000000000"]);
    await undeclare();
    expect(bus.undeclared).toEqual(["v1/*/state/parallax/alive"]);
  });

  it("emits an empty view when the fleet has no parallax at all, so the picker can say so", async () => {
    const bus = new FakeBus();
    const views: OriginsView[] = [];
    await watchOrigins(bus, (v) => views.push(v));
    expect(views).toHaveLength(1);
    expect(views[0]?.alive).toEqual([]);
  });
});
