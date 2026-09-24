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
import { describe } from "./rpc.js";
import type { StreamEnd, StreamStatus } from "./types.gen.js";

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
  log: $<HTMLPreElement>("log"),
};

let bus: Bus | undefined;
let stopOrigins: Undeclare | undefined;
let stopStatus: Undeclare | undefined;
let control: ControlPlane | undefined;
let catalogue: StreamDescriptor[] = [];
let statuses: ReadonlyMap<string, StreamStatus> = new Map();
/** What this page opened, per stream: the profile its close must name. */
const opened = new Map<string, Profile>();

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
  opened.clear();
  control = new ControlPlane(bus, origin, onSent);
  statuses = new Map();
  stopStatus = await watchStatus(bus, origin, (m) => {
    statuses = m;
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
      const outcome = await control!.switchTo(current, to);
      if (outcome.ok) opened.set(d.stream, to);
      render();
    });
    openVideo.disabled = !canVideo;
    const openPreview = button("open preview", async () => {
      const to = Profile.preview(d.stream);
      const outcome = await control!.switchTo(current, to);
      if (outcome.ok) opened.set(d.stream, to);
      render();
    });
    openPreview.disabled = !d.codecs.includes("mjpeg");
    const close = button("close", async () => {
      if (!current) return;
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

async function teardown(): Promise<void> {
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
  if (bus) await bus.close();
  bus = undefined;
  catalogue = [];
  statuses = new Map();
  render();
}

window.addEventListener("pagehide", () => {
  void teardown();
});
