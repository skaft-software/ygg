import { create } from "qrcode";

/** One row of the QR module matrix; `true` marks a dark module. */
export type QrMatrix = boolean[][];

/**
 * Encode a pairing ticket as a QR module matrix.
 *
 * Error-correction level M tolerates roughly 15 % module damage, which is
 * the realistic margin when a phone photographs the host's screen. The
 * largest QR symbol (version 40) holds at most 2331 bytes in byte mode at
 * level M — longer tickets may still encode when the encoder compresses
 * parts of them into denser numeric or alphanumeric segments — while the
 * protocol caps a ticket at 4096 bytes, so an invitation with many relay
 * or direct addresses can exceed QR capacity. The encoder throws for
 * such tickets; returning `null` lets the UI fall back to manual ticket
 * paste.
 */
export function pairingQrModules(ticket: string): QrMatrix | null {
  if (ticket.length === 0) return null;
  let code;
  try {
    code = create(ticket, { errorCorrectionLevel: "M" });
  } catch {
    return null;
  }
  const { size, data } = code.modules;
  const modules: QrMatrix = new Array(size);
  for (let row = 0; row < size; row += 1) {
    const line: boolean[] = new Array(size);
    for (let col = 0; col < size; col += 1) {
      line[col] = data[row * size + col] === 1;
    }
    modules[row] = line;
  }
  return modules;
}
