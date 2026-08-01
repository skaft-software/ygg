// Keep these values in lockstep with crates/ygg-coding-agent/src/tui/splash.rs.
export const TUI_SPLASH_DURATION_SECONDS = 2.2;
export const TUI_SPLASH_WIDTH = 24;
export const TUI_SPLASH_ROWS = 6;

type Rgb = readonly [number, number, number];

type Dot = {
  x: number;
  y: number;
  size: number;
};

type Pixel = {
  bits: number;
  red: number;
  green: number;
  blue: number;
  weight: number;
};

export type TuiSplashCell = {
  glyph: string;
  color: string | null;
};

const GRADIENT_STOPS: readonly Rgb[] = [
  [0x4b, 0x8d, 0xff],
  [0x45, 0xd9, 0xe8],
  [0x54, 0xe6, 0xb5],
  [0x8d, 0xff, 0x6a],
];

function dot(x: number, y: number, index: number): Dot {
  const variation = (Math.sin(index * 2.399_963) + 1) * 0.5;
  return {
    x,
    y,
    size: 1.5 + 1.5 * variation,
  };
}

function addCurve(
  dots: Dot[],
  count: number,
  curve: (progress: number) => readonly [number, number],
) {
  for (let index = 0; index < count; index += 1) {
    const progress = (index + 1) / (count + 1);
    const [x, y] = curve(progress);
    dots.push(dot(x, y, dots.length));
  }
}

function addMirroredCurve(
  dots: Dot[],
  count: number,
  curve: (progress: number) => readonly [number, number],
) {
  for (let index = 0; index < count; index += 1) {
    const progress = (index + 1) / (count + 1);
    const [x, y] = curve(progress);
    const right = dot(x, y, dots.length);
    dots.push(right, { ...right, x: -right.x });
  }
}

function geometry(): Dot[] {
  const dots: Dot[] = [];
  [0.22, 0.32, 0.42, 0.52, 0.62, 0.72].forEach((radiusY, band) => {
    const radiusX = radiusY * 1.18;
    const count = 11 + band * 6;
    for (let index = 0; index < count; index += 1) {
      const progress = (index + 0.5 * (band % 2)) / count;
      const angle = -2.92 + progress * 2.7;
      dots.push(
        dot(
          radiusX * Math.cos(angle),
          -0.23 + radiusY * Math.sin(angle),
          dots.length,
        ),
      );
    }
  });
  addCurve(dots, 18, (progress) => [0, 0.57 - progress * 1.02]);
  addMirroredCurve(dots, 12, (progress) => [
    0.52 * progress - 0.08 * progress * (1 - progress),
    0.05 - 0.57 * progress + 0.16 * progress * progress,
  ]);
  addCurve(dots, 7, (progress) => [0, -0.38 - 0.42 * progress]);
  addMirroredCurve(dots, 10, (progress) => [
    0.42 * progress,
    0.53 + 0.33 * progress - 0.08 * progress * (1 - progress),
  ]);
  addCurve(dots, 6, (progress) => [0, 0.58 + 0.35 * progress]);
  return dots;
}

const SPLASH_GEOMETRY = geometry();

function clamp(value: number, minimum: number, maximum: number) {
  return Math.min(maximum, Math.max(minimum, value));
}

function smoothstep(start: number, end: number, value: number) {
  const progress = clamp((value - start) / (end - start), 0, 1);
  return progress * progress * (3 - 2 * progress);
}

function mix(first: Rgb, second: Rgb, amount: number): Rgb {
  const channel = (left: number, right: number) =>
    left + (right - left) * amount;
  return [
    channel(first[0], second[0]),
    channel(first[1], second[1]),
    channel(first[2], second[2]),
  ];
}

function gradient(
  position: number,
  lift: number,
  modelAccent: Rgb,
): Rgb {
  const progress = clamp(1 - position, 0, 1) * 3;
  const index = Math.min(2, Math.floor(progress));
  const base = mix(
    GRADIENT_STOPS[index]!,
    GRADIENT_STOPS[index + 1]!,
    progress - index,
  );
  const adapted = mix(base, modelAccent, 0.58);
  return mix(adapted, [255, 255, 255], lift);
}

function brailleBit(x: number, y: number) {
  const bits = [
    [0x01, 0x02, 0x04, 0x40],
    [0x08, 0x10, 0x20, 0x80],
  ];
  return bits[x]?.[y] ?? 0;
}

function rustRound(value: number) {
  return value < 0 ? -Math.round(-value) : Math.round(value);
}

function parseHexColor(source: string): Rgb | null {
  const match = /^#([\da-f]{2})([\da-f]{2})([\da-f]{2})$/i.exec(source);
  if (!match) return null;
  return [
    Number.parseInt(match[1]!, 16),
    Number.parseInt(match[2]!, 16),
    Number.parseInt(match[3]!, 16),
  ];
}

function relativeLuminance(color: Rgb) {
  const channels = color.map((channel) => {
    const value = channel / 255;
    return value <= 0.04045
      ? value / 12.92
      : ((value + 0.055) / 1.055) ** 2.4;
  });
  return (
    channels[0]! * 0.2126 +
    channels[1]! * 0.7152 +
    channels[2]! * 0.0722
  );
}

function balanceForeground(source: Rgb, target: number): Rgb {
  const luminance = relativeLuminance(source);
  if (Math.abs(luminance - target) <= 0.002) return source;
  const destination: Rgb =
    luminance < target ? [255, 255, 255] : [0, 0, 0];
  let low = 0;
  let high = 1;
  for (let iteration = 0; iteration < 20; iteration += 1) {
    const amount = (low + high) / 2;
    const candidate = mix(source, destination, amount);
    const reached =
      luminance < target
        ? relativeLuminance(candidate) >= target
        : relativeLuminance(candidate) <= target;
    if (reached) high = amount;
    else low = amount;
  }
  return mix(source, destination, high);
}

function colorString(color: Rgb) {
  return `rgb(${color.map((channel) => Math.trunc(clamp(channel, 0, 255))).join(" ")})`;
}

/**
 * Port of the TUI's point-cloud rasterizer. It deliberately returns braille
 * cells, rather than tracing the tree into a generic vector silhouette.
 */
export function renderTuiSplashFrame(
  elapsed: number,
  modelAccentSource: string,
  width = TUI_SPLASH_WIDTH,
  rows = TUI_SPLASH_ROWS,
): { light: TuiSplashCell[]; dark: TuiSplashCell[] } {
  const safeWidth = clamp(Math.trunc(width), 0, 42);
  const safeRows = clamp(Math.trunc(rows), 0, 21);
  if (safeWidth === 0 || safeRows === 0) {
    return { light: [], dark: [] };
  }
  const source = parseHexColor(modelAccentSource) ?? [0x16, 0x87, 0x6d];
  const lightAccent = balanceForeground(source, 0.11);
  const darkAccent = balanceForeground(source, 0.27);
  const lightPixels = Array.from(
    { length: safeWidth * safeRows },
    (): Pixel => ({ bits: 0, red: 0, green: 0, blue: 0, weight: 0 }),
  );
  const darkPixels = lightPixels.map(
    (): Pixel => ({ bits: 0, red: 0, green: 0, blue: 0, weight: 0 }),
  );
  const time = clamp(elapsed, 0, TUI_SPLASH_DURATION_SECONDS);
  const subWidth = safeWidth * 2;
  const subHeight = safeRows * 4;
  const densityStride = safeRows <= 6 ? 2 : 1;

  SPLASH_GEOMETRY.forEach((point, index) => {
    if (safeRows <= 6 && index < 156 && index % densityStride !== 0) return;
    const vertical = clamp((point.y + 0.95) / 1.9, 0, 1);
    const revealStart = 0.15 + (1 - vertical) * 0.4;
    const reveal = smoothstep(revealStart, revealStart + 0.3, time);
    if (reveal <= 0.001) return;
    const travel = (1 - reveal) * 0.1;
    const settle =
      time >= 0.85 && time < 1.25
        ? 1 + 0.012 * Math.sin(((time - 0.85) / 0.4) * Math.PI)
        : 1;
    const diagonal = (point.x + 0.9 + (0.95 - point.y)) / 3.7;
    const front = clamp((time - 1.2) / 0.55, 0, 1);
    const ripple =
      time >= 1.2 && time < 1.75
        ? Math.exp(-(((diagonal - front) / 0.13) ** 2))
        : 0;
    const localScale = 0.7 + 0.3 * reveal;
    const x = point.x * settle;
    const y = (point.y + travel) * settle;
    const subX = rustRound(subWidth * 0.5 + x * subWidth * 0.43);
    const subY = rustRound(subHeight * 0.49 + y * subHeight * 0.47);
    const radius = Math.max(
      0.48,
      (point.size / 3) * localScale * (1 + ripple * 0.1),
    );
    const colors = [
      gradient(vertical, ripple * 0.2, lightAccent),
      gradient(vertical, ripple * 0.2, darkAccent),
    ] as const;
    const extent = Math.ceil(radius);

    for (let pixelY = subY - extent; pixelY <= subY + extent; pixelY += 1) {
      for (
        let pixelX = subX - extent;
        pixelX <= subX + extent;
        pixelX += 1
      ) {
        if (
          pixelX < 0 ||
          pixelY < 0 ||
          pixelX >= subWidth ||
          pixelY >= subHeight
        ) {
          continue;
        }
        const distance = Math.hypot(pixelX - subX, pixelY - subY);
        if (distance > radius + 0.15) continue;
        const cellX = Math.floor(pixelX / 2);
        const cellY = Math.floor(pixelY / 4);
        const bit = brailleBit(pixelX % 2, pixelY % 4);
        const alpha =
          reveal * Math.max(0.28, 1 - (distance / (radius + 0.5)) ** 2);
        [lightPixels, darkPixels].forEach((pixels, palette) => {
          const cell = pixels[cellY * safeWidth + cellX]!;
          const color = colors[palette]!;
          cell.bits |= bit;
          cell.red += color[0] * alpha;
          cell.green += color[1] * alpha;
          cell.blue += color[2] * alpha;
          cell.weight += alpha;
        });
      }
    }
  });

  const cells = (pixels: Pixel[]): TuiSplashCell[] =>
    pixels.map((pixel) => {
      if (pixel.bits === 0) return { glyph: "\u2800", color: null };
      const weight = Math.max(0.001, pixel.weight);
      return {
        glyph: String.fromCodePoint(0x2800 + pixel.bits),
        color: colorString([
          pixel.red / weight,
          pixel.green / weight,
          pixel.blue / weight,
        ]),
      };
    });

  return { light: cells(lightPixels), dark: cells(darkPixels) };
}
