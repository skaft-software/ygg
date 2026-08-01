import {
  type CSSProperties,
  useEffect,
  useMemo,
  useState,
} from "react";
import {
  renderTuiSplashFrame,
  TUI_SPLASH_DURATION_SECONDS,
} from "./tuiSplash";

function prefersStaticFrame() {
  const motion = document.documentElement.dataset.motion;
  return (
    motion === "none" ||
    motion === "reduced" ||
    (typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches)
  );
}

export function TuiSplashLogo({
  modelAccent,
}: {
  modelAccent: string;
}) {
  const [elapsed, setElapsed] = useState(() =>
    prefersStaticFrame() ? TUI_SPLASH_DURATION_SECONDS : 0,
  );

  useEffect(() => {
    if (prefersStaticFrame()) return;
    if (typeof window.requestAnimationFrame !== "function") {
      const timer = window.setTimeout(
        () => setElapsed(TUI_SPLASH_DURATION_SECONDS),
        0,
      );
      return () => window.clearTimeout(timer);
    }
    const startedAt = performance.now();
    let animationFrame = 0;
    const render = (now: number) => {
      const nextElapsed = Math.min(
        TUI_SPLASH_DURATION_SECONDS,
        (now - startedAt) / 1_000,
      );
      setElapsed(nextElapsed);
      if (nextElapsed < TUI_SPLASH_DURATION_SECONDS) {
        animationFrame = window.requestAnimationFrame(render);
      }
    };
    animationFrame = window.requestAnimationFrame(render);
    return () => window.cancelAnimationFrame(animationFrame);
  }, []);

  const frame = useMemo(
    () => renderTuiSplashFrame(elapsed, modelAccent),
    [elapsed, modelAccent],
  );

  return (
    <span
      className="tui-splash-logo"
      aria-hidden="true"
      data-animation={
        elapsed >= TUI_SPLASH_DURATION_SECONDS ? "settled" : "animating"
      }
    >
      {frame.light.map((lightCell, index) => {
        const darkCell = frame.dark[index]!;
        return (
          <span
            className="tui-splash-cell"
            key={index}
            style={
              {
                "--tui-splash-light": lightCell.color ?? "transparent",
                "--tui-splash-dark": darkCell.color ?? "transparent",
              } as CSSProperties
            }
          >
            {lightCell.glyph}
          </span>
        );
      })}
    </span>
  );
}
