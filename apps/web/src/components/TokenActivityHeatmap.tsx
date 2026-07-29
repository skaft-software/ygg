import type { UsageActivityDay } from "../protocol";

const DAY_MS = 86_400_000;
const WEEKS = 53;
const dayLabels = ["", "Mon", "", "Wed", "", "Fri", ""];
const compactNumber = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});
const fullNumber = new Intl.NumberFormat();
const dateLabel = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  year: "numeric",
  timeZone: "UTC",
});
const monthLabel = new Intl.DateTimeFormat(undefined, {
  month: "short",
  timeZone: "UTC",
});

function utcMidnight(date: Date): number {
  return Date.UTC(
    date.getUTCFullYear(),
    date.getUTCMonth(),
    date.getUTCDate(),
  );
}

function dateKey(timestamp: number): string {
  return new Date(timestamp).toISOString().slice(0, 10);
}

function activityLevel(tokens: number, maximum: number): number {
  if (tokens === 0 || maximum === 0) return 0;
  return Math.max(
    1,
    Math.ceil((Math.log1p(tokens) / Math.log1p(maximum)) * 4),
  );
}

export function TokenActivityHeatmap({
  days,
  today = new Date(),
}: {
  days: UsageActivityDay[];
  today?: Date;
}) {
  const todayMs = utcMidnight(today);
  const currentSunday = todayMs - new Date(todayMs).getUTCDay() * DAY_MS;
  const firstSunday = currentSunday - (WEEKS - 1) * 7 * DAY_MS;
  const activity = new Map(days.map((day) => [day.date, day]));
  const months = Array.from({ length: WEEKS }, (_, week) => {
    const timestamp = firstSunday + week * 7 * DAY_MS;
    const month = new Date(timestamp).getUTCMonth();
    const previousMonth = new Date(timestamp - 7 * DAY_MS).getUTCMonth();
    return week === 0 || month !== previousMonth
      ? monthLabel.format(timestamp)
      : "";
  });
  const cells = Array.from({ length: WEEKS * 7 }, (_, index) => {
    const timestamp = firstSunday + index * DAY_MS;
    const key = dateKey(timestamp);
    return { timestamp, key, day: activity.get(key) };
  });
  const maximum = cells.reduce(
    (value, cell) =>
      cell.timestamp <= todayMs ? Math.max(value, cell.day?.tokens ?? 0) : value,
    0,
  );

  return (
    <div className="token-heatmap">
      <div className="token-heatmap-scroll">
        <div className="token-heatmap-chart">
          <div className="token-heatmap-months" aria-hidden="true">
            {months.map((month, index) => (
              <span key={`${index}-${month}`}>{month}</span>
            ))}
          </div>
          <div className="token-heatmap-body">
            <div className="token-heatmap-days" aria-hidden="true">
              {dayLabels.map((label, index) => (
                <span key={`${index}-${label}`}>{label}</span>
              ))}
            </div>
            <div
              className="token-heatmap-cells"
              role="grid"
              aria-label="Daily token activity for the last 53 weeks"
              aria-rowcount={7}
              aria-colcount={WEEKS}
            >
              {cells.map(({ timestamp, key, day }, index) => {
                const future = timestamp > todayMs;
                const tokens = day?.tokens ?? 0;
                const description = future
                  ? `${dateLabel.format(timestamp)}: future date`
                  : tokens > 0
                    ? `${dateLabel.format(timestamp)}: ${fullNumber.format(tokens)} tokens across ${fullNumber.format(day?.requestCount ?? 0)} ${day?.requestCount === 1 ? "request" : "requests"}`
                    : `${dateLabel.format(timestamp)}: no recorded tokens`;
                return (
                  <span
                    className={`token-heatmap-cell ${future ? "is-future" : ""}`}
                    data-date={key}
                    data-level={future ? 0 : activityLevel(tokens, maximum)}
                    role="gridcell"
                    aria-rowindex={(index % 7) + 1}
                    aria-colindex={Math.floor(index / 7) + 1}
                    aria-label={description}
                    title={description}
                    key={key}
                  />
                );
              })}
            </div>
          </div>
        </div>
      </div>
      <div className="token-heatmap-legend" aria-label="Token activity scale">
        <span>Less</span>
        {[0, 1, 2, 3, 4].map((level) => (
          <span
            className="token-heatmap-cell"
            data-level={level}
            aria-hidden="true"
            key={level}
          />
        ))}
        <span>More</span>
        {maximum > 0 ? (
          <em>Peak {compactNumber.format(maximum)} tokens</em>
        ) : null}
      </div>
    </div>
  );
}
