// The page: bridge → origin → catalogue → open/close/keyframe, with the
// live status beside each stream. Everything except the pixels (#707).
import { connect, type Bus, type Undeclare } from "./bus.js";
import {
  fetchCatalogue,
  nativeLabel,
  tierChoices,
  watchStatus,
  type StreamDescriptor,
} from "./catalogue.js";
import { ControlPlane, Profile, type Sent } from "./control.js";
import { Origin } from "./keys.js";
import { watchOrigins } from "./origins.js";
import { PreviewTile } from "./preview.js";
import { maxLiveLatencyFrom } from "./receiver.js";
import { Reporter } from "./report.js";
import { describe } from "./rpc.js";
import { VideoTile } from "./tile.js";
import type { MediaReceiverReport, StreamEnd, StreamStatus } from "./types.gen.js";
import { WebCodecsDecoder, canvasPainter } from "./webcodecs.js";

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

const ui = {
  locator: $<HTMLInputElement>("locator"),
  connect: $<HTMLButtonElement>("connect"),
  link: $<HTMLSpanElement>("link"),
  origins: $<HTMLSelectElement>("origins"),
  originManual: $<HTMLInputElement>("origin-manual"),
  use: $<HTMLButtonElement>("use-origin"),
  reload: $<HTMLButtonElement>("reload"),
  streams: $<HTMLTableSectionElement>("streams"),
  tiles: $<HTMLDivElement>("tiles"),
  log: $<HTMLPreElement>("log"),
  mediaSession: $<HTMLSelectElement>("media-session"),
  deadline: $<HTMLInputElement>("deadline"),
};

let bus: Bus | undefined;
/** The `@media` session (#723): a second WebSocket so video cannot queue in front of control, or `bus` when shared. */
let mediaBus: Bus | undefined;
let reporter: Reporter | undefined;
let stopOrigins: Undeclare | undefined;
let stopStatus: Undeclare | undefined;
let control: ControlPlane | undefined;
let catalogue: StreamDescriptor[] = [];
let statuses: ReadonlyMap<string, StreamStatus> = new Map();
/** What this page opened, per stream: the profile its close must name. */
const opened = new Map<string, Profile>();

/** One live picture: the subscriber (its undeclare is the sensor's teardown signal) and the tile behind it. */
interface LiveTile {
  profile: Profile;
  stop: Undeclare;
  close(): void;
  root: HTMLDivElement;
  caption: HTMLDivElement;
}
const tiles = new Map<string, LiveTile>();

function log(line: string): void {
  const t = new Date().toISOString().slice(11, 19);
  ui.log.textContent = `${t}  ${line}\n${ui.log.textContent ?? ""}`.slice(0, 20_000);
}

function onSent(s: Sent): void {
  const c = s.command;
  const what =
    c.type === "request_keyframe"
      ? `keyframe ${c.stream}${c.tier ? `/${c.tier}` : ""}`
      : `${c.type === "open_stream" ? "open" : "close"} ${c.stream}/${c.codec ?? "?"}${c.tier ? `/${c.tier}` : ""}`;
  log(`${s.origin.value} ← ${what}: ${describe(s.outcome)}`);
}

ui.connect.addEventListener("click", async () => {
  await teardown();
  const locator = ui.locator.value.trim();
  ui.link.textContent = "connecting…";
  try {
    bus = await connect(locator);
  } catch (e) {
    ui.link.textContent = `failed: ${(e as Error).message}`;
    log(`connect ${locator}: ${(e as Error).message}`);
    return;
  }
  ui.link.textContent = `connected to ${locator}`;
  log(`connected to ${locator}`);
  if (ui.mediaSession.value === "separate") {
    try {
      mediaBus = await connect(locator);
      log("media session: a second WebSocket to the same bridge (#723)");
    } catch (e) {
      log(`media session failed, sharing the control session: ${(e as Error).message}`);
      mediaBus = bus;
    }
  } else {
    mediaBus = bus;
  }
  stopOrigins = await watchOrigins(bus, (view) => {
    const current = ui.origins.value;
    ui.origins.replaceChildren();
    if (view.alive.length === 0) {
      const o = document.createElement("option");
      o.value = "";
      o.textContent = "no parallax sensor is alive on this bus";
      ui.origins.append(o);
    }
    for (const origin of view.alive) {
      const o = document.createElement("option");
      o.value = origin.value;
      o.textContent = origin.value;
      ui.origins.append(o);
    }
    if ([...ui.origins.options].some((o) => o.value === current)) ui.origins.value = current;
  });
});

ui.use.addEventListener("click", () => {
  const raw = ui.originManual.value.trim() || ui.origins.value;
  const origin = Origin.parse(raw);
  if (!origin) {
    log(`refusing ${JSON.stringify(raw)}: not a host origin (h-<12 hex>); a stream control is never broadcast`);
    return;
  }
  void selectOrigin(origin);
});

ui.reload.addEventListener("click", () => {
  if (control) void loadCatalogue(control.origin);
});

async function selectOrigin(origin: Origin): Promise<void> {
  if (!bus) return;
  if (stopStatus) await stopStatus();
  await closeAllTiles();
  opened.clear();
  control = new ControlPlane(bus, origin, onSent);
  reporter = new Reporter(bus, origin, log);
  statuses = new Map();
  stopStatus = await watchStatus(bus, origin, (m) => {
    statuses = m;
    // The sensor closing a stream under a tile with no picture is one of the
    // three end conditions; a tile that has a picture rides out a status lag.
    for (const [stream, live] of tiles) {
      if (m.get(stream)?.open === false) live.close();
    }
    render();
  });
  await loadCatalogue(origin);
}

async function loadCatalogue(origin: Origin): Promise<void> {
  if (!bus) return;
  const cat = await fetchCatalogue(bus, origin);
  if (!cat) {
    log(`${origin.value}: the catalogue did not answer — is that host's parallax sensor up?`);
    catalogue = [];
  } else {
    catalogue = cat;
    log(`${origin.value}: ${cat.length} stream(s) in the catalogue`);
  }
  render();
}

function render(): void {
  ui.streams.replaceChildren();
  for (const d of catalogue) {
    const tr = document.createElement("tr");
    const status = statuses.get(d.stream);
    const current = opened.get(d.stream);

    const tiers = document.createElement("select");
    for (const t of tierChoices(d)) {
      const o = document.createElement("option");
      o.value = t;
      o.textContent = t;
      tiers.append(o);
    }
    if (current?.tier) tiers.value = current.tier;
    const canVideo = d.codecs.includes("h264") && tiers.options.length > 0;
    tiers.disabled = !canVideo;

    const openVideo = button("open video", async () => {
      const to = Profile.video(d.stream, tiers.value);
      // The outgoing tile's subscriber goes first (its falling edge is the
      // sensor's teardown signal), then close-then-open on the control plane.
      await stopTile(d.stream);
      const outcome = await control!.switchTo(current, to);
      if (outcome.ok) {
        opened.set(d.stream, to);
        await startTile(to);
      }
      render();
    });
    openVideo.disabled = !canVideo || !WebCodecsDecoder.available();
    if (!WebCodecsDecoder.available()) openVideo.title = "this browser has no WebCodecs VideoDecoder";
    const openPreview = button("open preview", async () => {
      const to = Profile.preview(d.stream);
      await stopTile(d.stream);
      const outcome = await control!.switchTo(current, to);
      if (outcome.ok) {
        opened.set(d.stream, to);
        await startTile(to);
      }
      render();
    });
    openPreview.disabled = !d.codecs.includes("mjpeg");
    const close = button("close", async () => {
      if (!current) return;
      await stopTile(d.stream);
      await control!.close(current);
      opened.delete(d.stream);
      render();
    });
    close.disabled = !current;
    const keyframe = button("keyframe", async () => {
      if (current) await control!.requestKeyframe(current);
    });
    keyframe.disabled = !current || current.codec !== "h264";

    tr.append(
      cell(d.stream, d.description ?? ""),
      cell(nativeLabel(d), d.codecs.join(", ")),
      cell(statusLabel(status, d), current ? `this page: ${current.codec}${current.tier ? `/${current.tier}` : ""}` : ""),
      cell(tiers),
      cell(openVideo, openPreview, close, keyframe),
    );
    ui.streams.append(tr);
  }
}

function statusLabel(s: StreamStatus | undefined, d: StreamDescriptor): string {
  if (!s) return d.active ? "active (no status yet)" : "closed";
  if (!s.open) {
    const end = s.last_end ? ` — last end: ${s.last_end.tier} ${endReason(s.last_end.reason)}` : "";
    return `closed${end}`;
  }
  const tiers = (s.tiers ?? [])
    .map((t) => `${t.tier} ${t.applied.width}×${t.applied.height}@${t.applied.fps} ${t.applied.bitrate_kbps}k, ${t.viewers} viewer(s)`)
    .join("; ");
  return `open: ${tiers || "no tier"}`;
}

function endReason(r: StreamEnd["reason"]): string {
  switch (r.type) {
    case "failed":
      return `failed${r.node ? ` in ${r.node}` : ""}: ${r.message}`;
    case "failed_open":
      return `failed to open: ${r.message}`;
    default:
      return r.type;
  }
}

function button(label: string, onClick: () => Promise<void>): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  b.addEventListener("click", () => {
    b.disabled = true;
    void onClick().finally(() => {
      b.disabled = false;
    });
  });
  return b;
}

function cell(...content: (string | Node)[]): HTMLTableCellElement {
  const td = document.createElement("td");
  for (const c of content) {
    if (typeof c === "string") {
      const div = document.createElement("div");
      div.textContent = c;
      td.append(div);
    } else {
      td.append(c, " ");
    }
  }
  return td;
}

/** Subscribe to the profile's EXACT media key and put a tile on the page. */
async function startTile(profile: Profile): Promise<void> {
  const mb = mediaBus ?? bus;
  if (!mb || !control || !reporter) return;
  const origin = control.origin;
  const root = document.createElement("div");
  root.className = "tile";
  const title = document.createElement("div");
  title.textContent = `${profile.stream} — ${profile.codec}${profile.tier ? `/${profile.tier}` : ""}`;
  const canvas = document.createElement("canvas");
  canvas.width = 16;
  canvas.height = 9;
  const caption = document.createElement("div");
  caption.className = "caption";
  caption.textContent = "waiting for frames…";
  root.append(title, canvas, caption);
  ui.tiles.append(root);

  const onReport = (r: MediaReceiverReport) => {
    reporter!.send(r);
    const age = r.frame_age_ms == null ? "age n/a" : `age ${r.frame_age_ms.toFixed(0)} ms`;
    const q = r.decoder_queue_depth == null ? "" : ` · queue ${r.decoder_queue_depth}`;
    caption.textContent = `rx ${r.received_frames} · lost ${r.lost_frames} · shed ${r.dropped_frames} · decoded ${r.decoded_frames} · ${age}${q}`;
  };
  const onEnded = (reason: string | undefined) => {
    caption.textContent = reason ?? "closed";
    if (reason) log(`${profile.stream}: ${reason}`);
  };

  if (profile.codec === "h264" && profile.tier) {
    const tier = profile.tier;
    let tile: VideoTile;
    const decoder = new WebCodecsDecoder(
      canvas,
      (d) => tile.onDecoded(d),
      (m) => tile.onDecodeError(m),
    );
    tile = new VideoTile({
      stream: profile.stream,
      tier,
      decoder,
      maxLiveLatencyMs: maxLiveLatencyFrom(Number(ui.deadline.value)),
      events: {
        requestKeyframe: () => void control!.requestKeyframe(profile),
        report: onReport,
        ended: onEnded,
        configured: (codec) => (title.textContent += ` · ${codec}`),
      },
    });
    const key = `v1/${origin.value}/@media/parallax/${profile.stream}/video/h264/${tier}`;
    const stop = await mb.subscribe(key, (s) =>
      tile.onSample({ payload: s.payload, attachment: s.attachment, publishedMs: s.publishedMs }),
    );
    // RFC 07 §1: the Nth viewer gets no matching-listener edge, so ask.
    void control.requestKeyframe(profile);
    tiles.set(profile.stream, { profile, stop, close: () => tile.close(), root, caption });
  } else {
    const tile = new PreviewTile({
      stream: profile.stream,
      paint: canvasPainter(canvas),
      events: { report: onReport, ended: onEnded },
    });
    const key = `v1/${origin.value}/@media/parallax/${profile.stream}/preview/jpeg`;
    const stop = await mb.subscribe(key, (s) =>
      tile.onSample({ payload: s.payload, attachment: s.attachment, publishedMs: s.publishedMs }),
    );
    tiles.set(profile.stream, { profile, stop, close: () => tile.close(), root, caption });
  }
}

/** Undeclare the stream's subscriber and drop its tile. The control-plane close is the caller's. */
async function stopTile(stream: string): Promise<void> {
  const live = tiles.get(stream);
  if (!live) return;
  tiles.delete(stream);
  live.close();
  await live.stop();
  live.root.remove();
}

async function closeAllTiles(): Promise<void> {
  for (const stream of [...tiles.keys()]) await stopTile(stream);
}

async function teardown(): Promise<void> {
  await closeAllTiles();
  // Close what this page opened, profile-correctly, before the session goes:
  // the sensor reaps on the subscriber's falling edge (#707), but a close it
  // was told about is a close it need not wait an idle window for.
  if (control) {
    for (const p of opened.values()) await control.close(p);
    opened.clear();
  }
  if (stopStatus) await stopStatus();
  if (stopOrigins) await stopOrigins();
  stopStatus = stopOrigins = undefined;
  control = undefined;
  if (mediaBus && mediaBus !== bus) await mediaBus.close();
  mediaBus = undefined;
  reporter = undefined;
  if (bus) await bus.close();
  bus = undefined;
  catalogue = [];
  statuses = new Map();
  render();
}

window.addEventListener("pagehide", () => {
  void teardown();
});
