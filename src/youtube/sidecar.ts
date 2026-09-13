// Argument: one video id. Prints one JSON line to stdout, diagnostics to stderr:
//   {"client":"VISIONOS","url":"https://...","contentLength":4175364}
// Exit 0 found, 1 failed, 2 usage.
import { Innertube, Platform, UniversalCache } from "youtubei.js";
// Types only; bgutils-js is loaded at runtime only when a token is needed.
import type { BG as BGTypes } from "bgutils-js";

// Required: without an evaluator every ciphered client fails.
// deno-lint-ignore require-await
(Platform as unknown as { shim: { eval: unknown } }).shim.eval = async (
  data: { output: string },
) => new Function(data.output)();

// youtubei.js logs to stdout, which must carry only the answer.
console.log = console.info = console.debug = console.error;

type TokenKind = "none" | "session" | "content";

type ClientName = NonNullable<NonNullable<Parameters<Innertube["getBasicInfo"]>[1]>["client"]>;

// Each client works only with this token kind; order measured.
const PLAN: Record<string, TokenKind> = {
  VISIONOS: "none",
  TV_SIMPLY: "session",
  YTMUSIC: "content",
};
const DEFAULT_ORDER = ["VISIONOS", "TV_SIMPLY", "YTMUSIC"];

// Test-only: reliably refused past the first megabyte.
const TEST_ONLY: Record<string, TokenKind> = { IOS: "none" };

/** Overrides the client order. For tests; unset in production. */
const CLIENTS_ENV = "TTSPOTIFY_SIDECAR_CLIENTS";

// Refused clients serve the start of a file, so probe its tail.
const PROBE_BYTES = 1024;

const PROBE_TIMEOUT_MS = 30_000;

const PROBE_ATTEMPTS = 3;
const RETRY_DELAY_MS = 500;

const STREAM_HEADERS = {
  accept: "*/*",
  origin: "https://www.youtube.com",
  referer: "https://www.youtube.com",
};

const REQUEST_KEY = "O43z0dpjhgX20SCx4KAo";

function log(msg: string): void {
  console.error(`[sidecar] ${msg}`);
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** A 4xx: retrying will not change it. */
class RefusedError extends Error {}

type Minter = { mintAsWebsafeString(identifier: string): Promise<string> };
type SignalOutput = NonNullable<Parameters<typeof BGTypes.WebPoMinter.create>[1]>;

let minter: Promise<Minter> | null = null;

/** One BotGuard attestation, made only when a client needs a token. */
function getMinter(visitorData: string): Promise<Minter> {
  minter ??= createMinter(visitorData);
  return minter;
}

async function createMinter(visitorData: string): Promise<Minter> {
  const { BG, USER_AGENT, buildURL, getHeaders } = await import("bgutils-js");
  const { JSDOM } = await import("jsdom");

  // BotGuard rejects a hand-rolled DOM stub; jsdom is required.
  const dom = new JSDOM("<!DOCTYPE html><html><body></body></html>", {
    url: "https://www.youtube.com/",
    referrer: "https://www.youtube.com/",
    userAgent: USER_AGENT,
    pretendToBeVisual: true,
  });
  Object.assign(globalThis, {
    window: dom.window,
    document: dom.window.document,
    location: dom.window.location,
    origin: dom.window.origin,
  });
  if (!Reflect.has(globalThis, "navigator")) {
    Object.defineProperty(globalThis, "navigator", { value: dom.window.navigator });
  }

  const bgConfig = {
    fetch: (input: RequestInfo | URL, init?: RequestInit) => fetch(input, init),
    globalObj: globalThis,
    requestKey: REQUEST_KEY,
    identifier: visitorData,
  };

  // deno-lint-ignore no-explicit-any
  const challenge = await BG.Challenge.create(bgConfig as any);
  if (!challenge) throw new Error("BotGuard returned no challenge");

  const interpreter = challenge.interpreterJavascript
    ?.privateDoNotAccessOrElseSafeScriptWrappedValue;
  if (!interpreter) throw new Error("BotGuard returned no interpreter");
  new Function(interpreter)();

  // Not BG.PoToken.generate: it discards the minter after one token.
  const botguard = await BG.BotGuardClient.create({
    program: challenge.program,
    globalName: challenge.globalName,
    globalObj: globalThis,
  });
  const webPoSignalOutput: SignalOutput = [];
  const botguardResponse = await botguard.snapshot({ webPoSignalOutput });
  const res = await fetch(buildURL("GenerateIT", false), {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify([REQUEST_KEY, botguardResponse]),
  });
  if (!res.ok) throw new Error(`integrity token request answered ${res.status}`);
  const [integrityToken, estimatedTtlSecs, mintRefreshThreshold, websafeFallbackToken] =
    await res.json();
  if (!integrityToken) throw new Error("BotGuard attestation was rejected");
  return await BG.WebPoMinter.create(
    { integrityToken, estimatedTtlSecs, mintRefreshThreshold, websafeFallbackToken },
    webPoSignalOutput,
  );
}

async function tokenFor(
  kind: TokenKind,
  client: string,
  videoId: string,
  visitorData: string,
): Promise<string | undefined> {
  if (kind === "none") return undefined;
  let m: Minter;
  try {
    m = await getMinter(visitorData);
  } catch (err) {
    throw new Error(`po-token mint failed: ${message(err)}`);
  }
  const token = await m.mintAsWebsafeString(kind === "session" ? visitorData : videoId);
  log(`${client}: token minted (${kind})`);
  return token;
}

/** Check one byte range is served exactly, retrying 5xx and network errors. */
async function probeRange(url: string, start: number, end: number): Promise<void> {
  let last: unknown;
  for (let attempt = 0; attempt < PROBE_ATTEMPTS; attempt++) {
    if (attempt > 0) {
      await new Promise((r) => setTimeout(r, RETRY_DELAY_MS * attempt));
    }
    try {
      const res = await fetch(url, {
        headers: { ...STREAM_HEADERS, range: `bytes=${start}-${end}` },
        signal: AbortSignal.timeout(PROBE_TIMEOUT_MS),
      });
      const body = new Uint8Array(await res.arrayBuffer());
      const detail = `HTTP ${res.status} at byte ${start}`;
      if (res.status >= 500) throw new Error(detail);
      if (!res.ok) throw new RefusedError(detail);
      if (res.status !== 206 || body.length !== end - start + 1) {
        throw new RefusedError(`range not honoured at byte ${start} (HTTP ${res.status}, ${body.length} bytes)`);
      }
      return;
    } catch (err) {
      if (err instanceof RefusedError) throw err;
      last = err;
    }
  }
  throw last;
}

type StreamInfo = { client: string; url: string; contentLength: number };

async function findStream(
  yt: Innertube,
  videoId: string,
  client: ClientName,
  kind: TokenKind,
  visitorData: string,
): Promise<StreamInfo> {
  const poToken = await tokenFor(kind, client, videoId, visitorData);

  // Options object required: a bare string silently falls back to WEB.
  const info = await yt.getBasicInfo(videoId, { client, po_token: poToken });

  const status = info.playability_status?.status;
  if (status && status !== "OK") {
    throw new Error(`playability ${status}`);
  }

  const audio = (info.streaming_data?.adaptive_formats ?? []).filter(
    (f) => f.has_audio && !f.has_video,
  );
  // itag 140: m4a 44.1kHz stereo, what the decoder expects.
  const fmt = audio.find((f) => f.itag === 140) ??
    audio
      .filter((f) => f.mime_type?.includes("audio/mp4"))
      .sort((a, b) => (b.bitrate ?? 0) - (a.bitrate ?? 0))[0];
  if (!fmt) throw new Error("no m4a audio format");

  // The session has no token, so decipher adds no pot; set this client's own.
  const url = new URL(await fmt.decipher(yt.session.player));
  if (poToken) url.searchParams.set("pot", poToken);
  const contentLength = Number(fmt.content_length ?? 0);
  if (!contentLength) throw new Error("format has no content_length");

  const probeStart = Math.max(0, contentLength - PROBE_BYTES);
  await probeRange(url.toString(), probeStart, contentLength - 1);

  return { client, url: url.toString(), contentLength };
}

/** A pipe may take a write in parts. */
async function writeAnswer(stream: StreamInfo): Promise<void> {
  const bytes = new TextEncoder().encode(`${JSON.stringify(stream)}\n`);
  let written = 0;
  while (written < bytes.length) {
    written += await Deno.stdout.write(bytes.subarray(written));
  }
}

function clientOrder(): [ClientName, TokenKind][] {
  const override = Deno.env.get(CLIENTS_ENV)?.trim();
  const names = override ? override.split(",").map((s) => s.trim()).filter(Boolean) : DEFAULT_ORDER;
  return names.map((name) => {
    const kind = PLAN[name] ?? TEST_ONLY[name];
    if (!kind) throw new Error(`${CLIENTS_ENV} names an unknown client: ${name}`);
    return [name as ClientName, kind];
  });
}

async function main(): Promise<number> {
  const videoId = Deno.args[0];
  if (!videoId) {
    log("usage: sidecar <video_id> [cookies_file]");
    return 2;
  }

  let order: [ClientName, TokenKind][];
  try {
    order = clientOrder();
  } catch (err) {
    log(message(err));
    return 2;
  }

  let yt: Innertube;
  try {
    yt = await Innertube.create({ cache: new UniversalCache(false) });
  } catch (err) {
    log(`session setup failed: ${message(err)}`);
    return 1;
  }
  const visitorData: string = yt.session.context.client.visitorData ?? "";

  let last = "";
  for (const [client, kind] of order) {
    try {
      const stream = await findStream(yt, videoId, client, kind, visitorData);
      log(`client=${client} contentLength=${stream.contentLength}`);
      await writeAnswer(stream);
      return 0;
    } catch (err) {
      last = `${client}: ${message(err)}`;
      log(last);
    }
  }
  log(`all clients failed; last: ${last}`);
  return 1;
}

Deno.exit(await main());
