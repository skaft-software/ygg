import type { QrMatrix } from "../pairing-qr";

/**
 * Draw a QR module matrix as an SVG symbol with a 4-module quiet zone —
 * the minimum the QR specification requires around the symbol.
 *
 * Dark modules render as one `<path>` (one subpath per module) so even the
 * largest ticket (version 40: 177 × 177 modules) stays a single DOM node.
 * The quiet zone and the fixed white/black colors are part of the QR
 * specification; a phone camera needs them regardless of the app theme.
 */
export function PairingQrCode({ modules }: { modules: QrMatrix }) {
  const size = modules.length;
  const quiet = 4;
  const span = size + quiet * 2;
  let path = "";
  modules.forEach((row, y) => {
    row.forEach((dark, x) => {
      if (dark) path += `M${x + quiet} ${y + quiet}h1v1h-1z`;
    });
  });
  return (
    <svg
      role="img"
      aria-label="Pairing code"
      viewBox={`0 0 ${span} ${span}`}
      className="qr-code"
    >
      <rect width={span} height={span} fill="#ffffff" />
      <path d={path} fill="#000000" />
    </svg>
  );
}
