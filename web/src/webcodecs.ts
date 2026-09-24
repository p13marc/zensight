// The browser halves the tests cannot touch: WebCodecs behind `Decoder`, and
// a `Painter` for the JPEG preview. Everything else in the tile is pure.
import type { Painter } from "./preview.js";
import type { Chunk, Decoded, Decoder } from "./tile.js";

/**
 * `VideoDecoder` as the tile's `Decoder`. Annex-B in: configured WITHOUT a
 * `description`, which is what tells WebCodecs the bitstream is Annex-B
 * rather than avcC. Every `VideoFrame` is closed on every path — painted,
 * shed because the tile is over, or errored.
 */
export class WebCodecsDecoder implements Decoder {
  private decoder: VideoDecoder | undefined;
  private codec: string | undefined;
  /** Sequences in flight, oldest first, so an output can be attributed (WebCodecs outputs carry only a timestamp). */
  private inFlight: { sequence: bigint; keyframe: boolean; timestamp: number }[] = [];
  private closed = false;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly onDecoded: (d: Decoded) => void,
    private readonly onError: (message: string) => void,
  ) {}

  static available(): boolean {
    return typeof VideoDecoder !== "undefined";
  }

  configure(codec: string): void {
    this.codec = codec;
    this.open();
  }

  private open(): void {
    if (this.closed || this.codec === undefined) return;
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* already closed */
      }
    }
    this.inFlight = [];
    const dec = new VideoDecoder({
      output: (frame) => this.paint(frame),
      error: (e) => this.onError(e.message),
    });
    dec.configure({ codec: this.codec, optimizeForLatency: true });
    this.decoder = dec;
  }

  private paint(frame: VideoFrame): void {
    try {
      if (this.closed) return;
      const canvas = this.canvas;
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      canvas.getContext("2d")?.drawImage(frame, 0, 0);
      // Attribute the output to the oldest in-flight AU with this timestamp,
      // or simply the oldest (all-zero timestamps when pts is absent).
      const i = this.inFlight.findIndex((f) => f.timestamp === frame.timestamp);
      const [meta] = this.inFlight.splice(i >= 0 ? i : 0, 1);
      if (meta) this.onDecoded({ sequence: meta.sequence, keyframe: meta.keyframe });
    } finally {
      frame.close();
    }
  }

  decode(chunk: Chunk): void {
    const dec = this.decoder;
    if (!dec || dec.state !== "configured") throw new Error(`decoder is ${dec?.state ?? "absent"}`);
    this.inFlight.push({ sequence: chunk.sequence, keyframe: chunk.type === "key", timestamp: chunk.timestamp });
    dec.decode(new EncodedVideoChunk({ type: chunk.type, timestamp: chunk.timestamp, data: chunk.data }));
  }

  reset(): void {
    // `VideoDecoder.reset()` drops the queue and the configuration; the
    // tile re-`configure`s right after, which reopens with the same codec.
    if (this.decoder && this.decoder.state !== "closed") this.decoder.reset();
    this.inFlight = [];
  }

  queueSize(): number {
    return this.decoder?.decodeQueueSize ?? 0;
  }

  close(): void {
    this.closed = true;
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {
        /* already closed */
      }
    }
    this.decoder = undefined;
    this.inFlight = [];
  }
}

/** A `Painter` that decodes a JPEG with `createImageBitmap` — no decoder object, no keyframe gate. */
export function canvasPainter(canvas: HTMLCanvasElement): Painter {
  return async (jpeg, width, height) => {
    const bitmap = await createImageBitmap(new Blob([jpeg as BlobPart], { type: "image/jpeg" }));
    try {
      const w = bitmap.width || width;
      const h = bitmap.height || height;
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }
      canvas.getContext("2d")?.drawImage(bitmap, 0, 0);
    } finally {
      bitmap.close();
    }
  };
}
