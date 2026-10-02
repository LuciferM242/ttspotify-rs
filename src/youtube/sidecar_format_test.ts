// Offline: choosing the audio format.
import { assertEquals } from "jsr:@std/assert@1";
import { pickAudioFormat } from "./sidecar.ts";

type Fmt = {
  itag: number;
  mime_type: string;
  bitrate: number;
  is_original: boolean;
  language?: string;
  audio_track?: { audio_is_default: boolean };
};

const fmt = (itag: number, extra: Partial<Fmt> = {}): Fmt => ({
  itag,
  mime_type: itag === 251 ? 'audio/webm; codecs="opus"' : 'audio/mp4; codecs="mp4a.40.2"',
  bitrate: itag === 139 ? 50_000 : 130_000,
  is_original: true,
  ...extra,
});

Deno.test("a single-track video plays itag 140", () => {
  const picked = pickAudioFormat([fmt(139), fmt(140), fmt(251)]);
  assertEquals(picked?.itag, 140);
});

Deno.test("the original language wins over dubs listed before it", () => {
  const dub = (lang: string) => ({ is_original: false, language: lang });
  const picked = pickAudioFormat([
    fmt(140, dub("ar")),
    fmt(140, dub("de")),
    fmt(140, { language: "en-US" }),
    fmt(251, dub("ar")),
  ]);
  assertEquals(picked?.language, "en-US");
});

Deno.test("the default track is used when none is marked original", () => {
  const picked = pickAudioFormat([
    fmt(140, { is_original: false, language: "ar", audio_track: { audio_is_default: false } }),
    fmt(140, { is_original: false, language: "en", audio_track: { audio_is_default: true } }),
  ]);
  assertEquals(picked?.language, "en");
});

Deno.test("with no original or default track, any m4a still plays", () => {
  const picked = pickAudioFormat([fmt(251), fmt(140, { is_original: false })]);
  assertEquals(picked?.itag, 140);
});

Deno.test("without itag 140 the best original m4a is chosen", () => {
  const picked = pickAudioFormat([
    fmt(139),
    fmt(141, { bitrate: 260_000, is_original: false }),
    fmt(145, { bitrate: 120_000 }),
  ]);
  assertEquals(picked?.itag, 145);
});

Deno.test("no m4a at all picks nothing", () => {
  assertEquals(pickAudioFormat([fmt(251)]), undefined);
});
