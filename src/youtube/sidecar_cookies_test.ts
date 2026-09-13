// Offline: reading a cookies file.
import { assert, assertEquals } from "jsr:@std/assert@1";
import { isSignedIn, parseCookieFile } from "./sidecar.ts";

const row = (domain: string, name: string, value: string) =>
  [domain, "TRUE", "/", "TRUE", "0", name, value].join("\t");

Deno.test("login cookies are kept, and YouTube's copy beats Google's", () => {
  const file = [
    "# Netscape HTTP Cookie File",
    row(".google.com", "SID", "fromgoogle"),
    row(".youtube.com", "SID", "fromyoutube"),
    `#HttpOnly_${row(".youtube.com", "SAPISID", "secret")}`,
    row(".youtube.com", "NOT_A_LOGIN_COOKIE", "x"),
    row(".example.com", "HSID", "elsewhere"),
    row("notyoutube.com", "SSID", "lookalike"),
    "not a cookie line",
  ].join("\r\n");
  const header = parseCookieFile(file);
  assertEquals(header, "SID=fromyoutube; SAPISID=secret");
  assert(isSignedIn(header));
});

Deno.test("the secure SAPISID stands in when the plain one is missing", () => {
  const header = parseCookieFile(row(".youtube.com", "__Secure-3PAPISID", "sec"));
  assertEquals(header, "__Secure-3PAPISID=sec; SAPISID=sec");
  assert(isSignedIn(header));
});

Deno.test("an export made while signed out is not a sign-in", () => {
  const header = parseCookieFile(
    [row(".youtube.com", "YSC", "a"), row(".youtube.com", "VISITOR_INFO1_LIVE", "b")].join("\n"),
  );
  assertEquals(header, "YSC=a; VISITOR_INFO1_LIVE=b");
  assert(!isSignedIn(header));
  assertEquals(parseCookieFile(""), "");
  assert(!isSignedIn(parseCookieFile(row(".youtube.com", "SAPISID", ""))));
});
