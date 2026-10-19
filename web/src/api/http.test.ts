import { afterEach, describe, expect, it, vi } from "vitest";
import { http, setSignedOutHandler } from "./http";

type Call = { url: string; init: RequestInit };

function server(responses: Response[]) {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit) => {
      calls.push({ url, init });
      return responses.shift() ?? new Response(null, { status: 204 });
    }),
  );
  return calls;
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

const signedIn = {
  configured: true,
  setup_pending: false,
  locked: null,
  min_password_length: 8,
  authenticated: true,
  csrf_token: "c".repeat(64),
  session: { id: "0123456789abcdef", expires_at_ms: 0 },
  api_version: 1,
};

afterEach(() => {
  vi.unstubAllGlobals();
  setSignedOutHandler(null);
});

describe("HTTP backend", () => {
  it("sends the session's CSRF token with changes only", async () => {
    const calls = server([json(signedIn), json({}), new Response(null, { status: 204 })]);
    await http.auth();
    await http.overview();
    await http.approvePairing("client", "request", "1234", "Laptop");
    const headers = (call: Call) => call.init.headers as Record<string, string>;
    expect(headers(calls[1])["X-CSRF-Token"]).toBeUndefined();
    expect(calls[2].url).toBe("/api/v1/pairing/approve");
    expect(calls[2].init.method).toBe("POST");
    expect(headers(calls[2])["X-CSRF-Token"]).toBe("c".repeat(64));
    expect(JSON.parse(calls[2].init.body as string)).toEqual({ client_id: "client", request: "request", pin: "1234", label: "Laptop" });
    expect(calls.every((call) => call.init.credentials === "same-origin")).toBe(true);
  });

  it("reports server errors and an ended session", async () => {
    const signedOut = vi.fn();
    setSignedOutHandler(signedOut);
    server([
      json({ error: { kind: "conflict", message: "The configuration file changed." } }, 409),
      json({ error: { kind: "unauthenticated", message: "Sign in to manage Pyroshine." } }, 401),
    ]);
    await expect(http.saveConfig({}, "r1")).rejects.toEqual({ kind: "conflict", message: "The configuration file changed." });
    expect(signedOut).not.toHaveBeenCalled();
    await expect(http.overview()).rejects.toMatchObject({ kind: "unauthenticated" });
    expect(signedOut).toHaveBeenCalledOnce();
  });

  it("turns a network failure into an unavailable error", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Promise.reject(new TypeError("Failed to fetch"))));
    await expect(http.overview()).rejects.toMatchObject({ kind: "unavailable" });
  });

  it("names a support bundle after the server's attachment", async () => {
    server([
      new Response(new Uint8Array([0x1f, 0x8b]), {
        status: 200,
        headers: { "Content-Disposition": 'attachment; filename="pyroshine-diag-20261016-002625.tar.gz"' },
      }),
    ]);
    const bundle = await http.supportBundle();
    expect(bundle.name).toBe("pyroshine-diag-20261016-002625.tar.gz");
    expect(bundle.blob.size).toBe(2);
  });
});
