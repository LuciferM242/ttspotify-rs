// Live tests: they talk to YouTube on purpose.
import { assert, assertEquals } from "jsr:@std/assert@1";
import { fromFileUrl } from "jsr:@std/path@1";

// Not .pathname: on Windows that yields "/C:/...", which no process can open.
const HERE = fromFileUrl(new URL(".", import.meta.url));
const SCRIPT = `${HERE}sidecar.ts`;
// Named explicitly, as the bot does: Deno looks for config from the cwd, not the script.
const CONFIG = `${HERE}sidecar_deno.json`;
const LOCK = `${HERE}sidecar_deno.lock`;

const TOPIC = "5oWyMakvQew";

const STREAM_HEADERS = {
  accept: "*/*",
  origin: "https://www.youtube.com",
  referer: "https://www.youtube.com",
};

type StreamInfo = { client: string; url: string; contentLength: number };

async function run(
  videoId?: string,
  clients?: string,
): Promise<{ code: number; stdout: string; stderr: string }> {
  const args = [
    "run", "--allow-net", "--allow-read", "--allow-write", "--allow-env",
    "--config", CONFIG, "--lock", LOCK, "--frozen", SCRIPT,
  ];
  if (videoId !== undefined) args.push(videoId);
  const env: Record<string, string> = {};
  if (clients !== undefined) env.TTSPOTIFY_SIDECAR_CLIENTS = clients;
  const out = await new Deno.Command("deno", { args, env, stdout: "piped", stderr: "piped" }).output();
  const decode = (b: Uint8Array) => new TextDecoder().decode(b);
  return { code: out.code, stdout: decode(out.stdout), stderr: decode(out.stderr) };
}

function streamInfo(stdout: string): StreamInfo {
  const line = stdout.split("\n").map((l) => l.trim()).filter(Boolean).at(-1);
  assert(line, "the sidecar printed nothing on stdout");
  const info = JSON.parse(line) as StreamInfo;
  assert(info.url.startsWith("https://"), `not an https url: ${info.url}`);
  assert(info.contentLength > 0, `no content length: ${line}`);
  return info;
}

Deno.test("returns stream info for a plain video", async () => {
  const r = await run("dQw4w9WgXcQ");
  assertEquals(r.code, 0, r.stderr);
  const info = streamInfo(r.stdout);
  assert(info.client.length > 0, r.stdout);
  assert(r.stderr.includes("[sidecar] client="), r.stderr);
});

Deno.test("a '- Topic' track's url downloads completely in ranges", async () => {
  // The probe checks only the tail; this proves every byte is served.
  const r = await run(TOPIC);
  assertEquals(r.code, 0, r.stderr);
  const info = streamInfo(r.stdout);
  const chunk = 1048576;
  let bytes = 0;
  while (bytes < info.contentLength) {
    const end = Math.min(bytes + chunk, info.contentLength) - 1;
    const res = await fetch(info.url, { headers: { ...STREAM_HEADERS, range: `bytes=${bytes}-${end}` } });
    assertEquals(res.status, 206, `range at byte ${bytes}`);
    const body = new Uint8Array(await res.arrayBuffer());
    assertEquals(body.length, end - bytes + 1, `short range at byte ${bytes}`);
    bytes += body.length;
  }
  assertEquals(bytes, info.contentLength);
});

Deno.test("a track VISIONOS serves never mints a token", async () => {
  const r = await run(TOPIC);
  assertEquals(r.code, 0, r.stderr);
  assertEquals(streamInfo(r.stdout).client, "VISIONOS", r.stderr);
  assert(!r.stderr.includes("token minted"), r.stderr);
});

Deno.test("TV_SIMPLY is found with the session token", async () => {
  const r = await run(TOPIC, "TV_SIMPLY");
  assertEquals(r.code, 0, r.stderr);
  assertEquals(streamInfo(r.stdout).client, "TV_SIMPLY");
  assert(r.stderr.includes("token minted (session)"), r.stderr);
});

Deno.test("YTMUSIC is found with a per-video token", async () => {
  const r = await run(TOPIC, "YTMUSIC");
  assertEquals(r.code, 0, r.stderr);
  assertEquals(streamInfo(r.stdout).client, "YTMUSIC");
  assert(r.stderr.includes("token minted (content)"), r.stderr);
});

Deno.test("a client refused past the first megabyte is never handed over", async () => {
  const r = await run(TOPIC, "IOS");
  assertEquals(r.code, 1, r.stderr);
  assertEquals(r.stdout.trim(), "", r.stderr);
  assert(r.stderr.includes("[sidecar] IOS:"), r.stderr);
  assert(r.stderr.includes("[sidecar] all clients failed"), r.stderr);
});

Deno.test("a refused client falls through to the next", async () => {
  const r = await run(TOPIC, "IOS,VISIONOS");
  assertEquals(r.code, 0, r.stderr);
  assert(r.stderr.includes("[sidecar] IOS:"), r.stderr);
  assertEquals(streamInfo(r.stdout).client, "VISIONOS");
});

Deno.test("reports failure for a bogus id", async () => {
  const r = await run("aaaaaaaaaaa");
  assertEquals(r.code, 1);
  assertEquals(r.stdout.trim(), "");
  assert(r.stderr.includes("[sidecar] all clients failed"), r.stderr);
});

Deno.test("an unknown client in the override is a usage error", async () => {
  const r = await run(TOPIC, "NOT_A_CLIENT");
  assertEquals(r.code, 2, r.stderr);
  assert(r.stderr.includes("unknown client: NOT_A_CLIENT"), r.stderr);
});

Deno.test("rejects missing arguments", async () => {
  const r = await run();
  assertEquals(r.code, 2);
});
