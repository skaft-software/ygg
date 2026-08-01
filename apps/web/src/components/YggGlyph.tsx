import yggGlyphUrl from "../assets/ygg-glyph.svg";

export function YggGlyph({ className = "" }: { className?: string }) {
  return (
    <img
      className={`ygg-glyph ${className}`.trim()}
      src={yggGlyphUrl}
      alt="ygg"
    />
  );
}
