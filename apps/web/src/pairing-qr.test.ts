import { createRequire } from "node:module";
import jsQR from "jsqr";
import { describe, expect, it } from "vitest";
import { pairingQrModules } from "./pairing-qr";

// A realistic invitation: one host with two relay endpoints and two direct
// addresses, base64url-encoded. 525 bytes total — inside the protocol's
// 4096-byte ticket cap. The encoder's mode switching (numeric,
// alphanumeric, and byte segments) places this ticket at QR version 18
// (89 modules) with error-correction level M.
const REALISTIC_TICKET =
  "ygg://pair/v1/eyJwcm90b2NvbCI6MSwiaG9zdElkIjoiYTFiMmMzZDRlNWY2YTdiOGM5ZDBlMWYyYTNiNGM1ZDYiLCJob3N0RW5kcG9pbnRJZCI6IndUYmpONFJaOWNLalh5WnZLcVBtTDhzRDJmRzd1QTFiQzNlSDVpSjRvSzYiLCJyZWxheVVybHMiOlsid3NzOi8vcmVsYXkxLnlnZy5leGFtcGxlL2NvbXBhbmlvbiIsIndzczovL3JlbGF5Mi55Z2cuZXhhbXBsZS9jb21wYW5pb24iXSwiZGlyZWN0QWRkcmVzc2VzIjpbIjE5Mi4xNjguMS4yNDo3NDExIiwiW2ZkMTI6MzQ1Njo3ODlhOjoxXTo3NDExIl0sImludml0YXRpb24iOiI5ZjhlN2Q2YzViNGEzOTI4MTcwNmY1ZTRkM2MyYjFhMDk4ODc3NjY1NTQ0MzMyMjExMDBmZmVlZGRjY2JiYWEiLCJleHBpcmVzQXRNcyI6MTc2MDAwMDAwMDAwMH0";

/**
 * Render a module matrix as a raw RGBA buffer, padded with the QR
 * specification's 4-module quiet zone — the white border a phone camera
 * actually sees when the code sits on the dialog's white background. jsQR
 * needs the full quiet zone for small symbols: it refuses to decode a
 * version 1 symbol flush against the image edge.
 */
function rgbaWithQuietZone(modules: boolean[][]): {
  rgba: Uint8ClampedArray;
  width: number;
} {
  const quiet = 4;
  const width = modules.length + quiet * 2;
  const rgba = new Uint8ClampedArray(width * width * 4).fill(255);
  modules.forEach((row, y) => {
    row.forEach((dark, x) => {
      if (!dark) return;
      const offset = ((y + quiet) * width + (x + quiet)) * 4;
      rgba[offset] = 0;
      rgba[offset + 1] = 0;
      rgba[offset + 2] = 0;
      rgba[offset + 3] = 255;
    });
  });
  return { rgba, width };
}

type VendoredDecoder = (
  data: Uint8ClampedArray,
  width: number,
  height: number,
) => { data: string } | null;

describe("pairingQrModules", () => {
  it("round-trips a realistic ticket through the bundled decoder", () => {
    const modules = pairingQrModules(REALISTIC_TICKET);
    expect(modules).not.toBeNull();
    const size = modules!.length;
    for (const row of modules!) expect(row).toHaveLength(size);
    // Level M places this 525-byte ticket at QR version 18 (89 modules).
    expect(size).toBeLessThanOrEqual(89);
    const { rgba, width } = rgbaWithQuietZone(modules!);
    expect(jsQR(rgba, width, width)?.data).toBe(REALISTIC_TICKET);
  });

  it("encodes a short ticket at a small QR version", () => {
    const modules = pairingQrModules("ygg://pair/v1/example");
    expect(modules).not.toBeNull();
    const size = modules!.length;
    expect(size).toBeLessThanOrEqual(41); // version 5 or smaller
    const { rgba, width } = rgbaWithQuietZone(modules!);
    expect(jsQR(rgba, width, width)?.data).toBe("ygg://pair/v1/example");
  });

  it("returns null once the ticket outgrows QR capacity", () => {
    // Version 40 at level M holds 2331 bytes in byte mode; lowercase
    // payloads force byte mode, so these overflow it (2332 and 4096 total).
    expect(pairingQrModules(`ygg://pair/v1/${"a".repeat(2318)}`)).toBeNull();
    expect(pairingQrModules(`ygg://pair/v1/${"a".repeat(4082)}`)).toBeNull();
    expect(pairingQrModules("")).toBeNull();
  });

  it("keeps the vendored scanner copy decoding the same symbols", () => {
    // The companion app ships its own copy of jsQR inside the native bundle
    // (src-tauri/vendor/jsqr.js). Loading the exact vendored bytes here
    // proves the on-device decoder reads what the host generates, without
    // relying on the node_modules copy.
    const require = createRequire(import.meta.url);
    const vendored: unknown = require("../../mobile/src-tauri/vendor/jsqr.js");
    const decode: VendoredDecoder | undefined =
      typeof vendored === "function"
        ? (vendored as VendoredDecoder)
        : (vendored as { default?: VendoredDecoder } | undefined)?.default;
    expect(decode).toBeTypeOf("function");
    if (typeof decode !== "function") {
      throw new Error("vendored jsQR is not callable");
    }
    const modules = pairingQrModules(REALISTIC_TICKET)!;
    const { rgba, width } = rgbaWithQuietZone(modules);
    expect(decode(rgba, width, width)?.data).toBe(REALISTIC_TICKET);
  });
});
