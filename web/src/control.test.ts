import { describe, expect, it } from "vitest";
import { ControlPlane, Profile, type Sent } from "./control.js";
import { FakeBus, err, ok } from "./fake.js";
import { Origin } from "./keys.js";

const origin = Origin.of("h-3fa9c2d41b7e");

describe("the close is profile-correct (trap 1)", () => {
  it("a video tier's close names its codec AND its tier", () => {
    expect(Profile.video("cam0", "high").closeCommand()).toEqual({
      type: "close_stream",
      stream: "cam0",
      codec: "h264",
      tier: "high",
    });
  });

  it("the preview's close names mjpeg and no tier — its key has no tier chunk", () => {
    expect(Profile.preview("cam0").closeCommand()).toEqual({
      type: "close_stream",
      stream: "cam0",
      codec: "mjpeg",
    });
  });

  it("an open looks exactly like the sensor's documented example", () => {
    // `{"type":"open_stream","stream":"cam0","tier":"high"}` plus the codec,
    // which the browser always names so the sensor's default is never relied on.
    expect(JSON.parse(JSON.stringify(Profile.video("cam0", "high").openCommand()))).toEqual({
      type: "open_stream",
      stream: "cam0",
      codec: "h264",
      tier: "high",
    });
  });
});

describe("commands ride the write procedure as Command<StreamControl>", () => {
  it("sends JSON with an id to the origin's stream/set key and reads an empty reply as executed", async () => {
    const bus = new FakeBus();
    const sent: Sent[] = [];
    const cp = new ControlPlane(bus, origin, (s) => sent.push(s));
    const outcome = await cp.open(Profile.video("cam0", "low"));
    expect(outcome).toEqual({ ok: true });
    expect(bus.gets).toHaveLength(1);
    expect(bus.gets[0]?.selector).toBe("v1/h-3fa9c2d41b7e/@rpc/parallax/stream/set");
    expect(bus.gets[0]?.body).toEqual({
      id: "web-1",
      body: { type: "open_stream", stream: "cam0", codec: "h264", tier: "low" },
    });
    expect(sent[0]?.outcome.ok).toBe(true);
  });

  it("a reply_err carrying RpcError is a refusal with the sensor's own words", async () => {
    const bus = new FakeBus();
    bus.answer = () => [err({ error: "invalid_args", message: "no such stream: cam9" })];
    const cp = new ControlPlane(bus, origin);
    const outcome = await cp.open(Profile.video("cam9", "low"));
    expect(outcome).toEqual({
      ok: false,
      error: { error: "invalid_args", message: "no such stream: cam9" },
    });
  });

  it("no reply at all is 'unanswered', not success", async () => {
    const bus = new FakeBus();
    bus.answer = () => [];
    const cp = new ControlPlane(bus, origin);
    const outcome = await cp.open(Profile.preview("cam0"));
    expect(outcome.ok).toBe(false);
    expect("unanswered" in outcome && outcome.unanswered).toBe(true);
  });
});

describe("close-then-open, in arrival order (trap 1, second half)", () => {
  it("switching tiers sends the OLD profile's close before the new open, serially", async () => {
    const bus = new FakeBus();
    bus.latencyMs = 5;
    const cp = new ControlPlane(bus, origin);
    const outcome = await cp.switchTo(Profile.video("cam0", "low"), Profile.video("cam0", "high"));
    expect(outcome.ok).toBe(true);
    expect(bus.gets.map((g) => g.body)).toEqual([
      { id: "web-1", body: { type: "close_stream", stream: "cam0", codec: "h264", tier: "low" } },
      { id: "web-2", body: { type: "open_stream", stream: "cam0", codec: "h264", tier: "high" } },
    ]);
  });

  it("two callers racing on one control plane are still serialised", async () => {
    const bus = new FakeBus();
    bus.latencyMs = 5;
    const started: string[] = [];
    bus.answer = (g) => {
      started.push((g.body as { id: string }).id);
      return [ok("")];
    };
    const cp = new ControlPlane(bus, origin);
    await Promise.all([
      cp.close(Profile.video("cam0", "low")),
      cp.open(Profile.video("cam0", "high")),
      cp.requestKeyframe(Profile.video("cam0", "high")),
    ]);
    expect(started).toEqual(["web-1", "web-2", "web-3"]);
  });

  it("a refused close does not stop the open, and a failed link does not poison the queue", async () => {
    const bus = new FakeBus();
    let n = 0;
    bus.answer = () => (++n === 1 ? [err({ error: "x", message: "closed already" })] : [ok("")]);
    const cp = new ControlPlane(bus, origin);
    const outcome = await cp.switchTo(Profile.video("cam0", "low"), Profile.video("cam0", "high"));
    expect(outcome).toEqual({ ok: true });
    expect(bus.gets).toHaveLength(2);
    // And the queue is still usable afterwards.
    bus.answer = () => {
      throw new Error("bus down");
    };
    await expect(cp.open(Profile.preview("cam0"))).rejects.toThrow("bus down");
    bus.answer = () => [ok("")];
    expect(await cp.open(Profile.preview("cam0"))).toEqual({ ok: true });
  });

  it("re-selecting the same profile asks for a keyframe instead of close+open (the Nth viewer rule)", async () => {
    const bus = new FakeBus();
    const cp = new ControlPlane(bus, origin);
    const p = Profile.video("cam0", "high");
    await cp.switchTo(p, Profile.video("cam0", "high"));
    expect(bus.gets.map((g) => (g.body as { body: unknown }).body)).toEqual([
      { type: "request_keyframe", stream: "cam0", tier: "high" },
    ]);
  });
});
