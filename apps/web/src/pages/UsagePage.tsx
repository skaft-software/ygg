import { RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import type {
  LifetimeUsage,
  UsageActivity,
  UsageBreakdown,
  UsagePeriod,
  UsageStats,
} from "../protocol";
import { TokenActivityHeatmap } from "../components/TokenActivityHeatmap";

const compactNumber = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});
const fullNumber = new Intl.NumberFormat();
const percentNumber = new Intl.NumberFormat(undefined, {
  style: "percent",
  maximumFractionDigits: 1,
});
const dateFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeZone: "UTC",
});

const ranges = ["daily", "weekly", "all"] as const;
type UsageRange = (typeof ranges)[number];

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message
    ? error.message
    : "Usage metrics are temporarily unavailable.";
}

function metricTitle(value: number): string {
  return fullNumber.format(value);
}

function lifetimeRange(lifetime: LifetimeUsage): string {
  if (
    lifetime.firstRequestAtMs === undefined ||
    lifetime.lastRequestAtMs === undefined
  ) {
    return "No inference requests recorded yet";
  }
  return `${dateFormat.format(lifetime.firstRequestAtMs)} – ${dateFormat.format(lifetime.lastRequestAtMs)}`;
}

function rangeLabel(range: UsageRange): string {
  switch (range) {
    case "daily":
      return "Today";
    case "weekly":
      return "Last 7 days";
    case "all":
      return "All time";
  }
}

function requestLabel(requestCount: number): string {
  if (requestCount === 0) return "No completed requests";
  return `${fullNumber.format(requestCount)} completed ${requestCount === 1 ? "request" : "requests"}`;
}

function summaryDetail(
  range: UsageRange,
  usage: UsageBreakdown,
  lifetime: LifetimeUsage | null,
): string {
  if (range !== "all" || !lifetime) return requestLabel(usage.requestCount);
  if (usage.requestCount === 0) return lifetimeRange(lifetime);
  return `${requestLabel(usage.requestCount)} · ${lifetimeRange(lifetime)}`;
}

function modelName(value: string): string {
  return value === "unknown" ? "Unknown model" : value;
}

function providerName(value: string): string {
  return value === "unknown" ? "Unknown provider" : value;
}

interface UsagePageProps {
  loadStats: (period: UsagePeriod) => Promise<UsageStats>;
  loadLifetime: () => Promise<LifetimeUsage>;
  loadActivity: () => Promise<UsageActivity>;
}

export function UsagePage({
  loadStats,
  loadLifetime,
  loadActivity,
}: UsagePageProps) {
  const [range, setRange] = useState<UsageRange>("daily");
  const [stats, setStats] = useState<UsageStats | null>(null);
  const [lifetime, setLifetime] = useState<LifetimeUsage | null>(null);
  const [activity, setActivity] = useState<UsageActivity | null>(null);
  const [statsLoading, setStatsLoading] = useState(true);
  const [overviewLoading, setOverviewLoading] = useState(true);
  const [statsError, setStatsError] = useState<string | null>(null);
  const [overviewError, setOverviewError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (range === "all") return;
    let cancelled = false;
    void loadStats(range)
      .then((result) => {
        if (!cancelled) setStats(result);
      })
      .catch((error: unknown) => {
        if (!cancelled) setStatsError(errorMessage(error));
      })
      .finally(() => {
        if (!cancelled) setStatsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [loadStats, range, refreshKey]);

  useEffect(() => {
    let cancelled = false;
    void Promise.all([loadLifetime(), loadActivity()])
      .then(([nextLifetime, nextActivity]) => {
        if (cancelled) return;
        setLifetime(nextLifetime);
        setActivity(nextActivity);
      })
      .catch((error: unknown) => {
        if (!cancelled) setOverviewError(errorMessage(error));
      })
      .finally(() => {
        if (!cancelled) setOverviewLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [loadActivity, loadLifetime, refreshKey]);

  const visibleUsage: UsageBreakdown | null =
    range === "all"
      ? lifetime
      : stats?.period === range
        ? stats
        : null;
  const usageLoading =
    range === "all"
      ? overviewLoading && !visibleUsage
      : statsLoading && !visibleUsage;
  const error = statsError ?? overviewError;
  const label = rangeLabel(range);

  const selectRange = (nextRange: UsageRange) => {
    if (nextRange === range) return;
    setStatsError(null);
    if (nextRange !== "all") setStatsLoading(true);
    setRange(nextRange);
  };

  const retry = () => {
    setStatsLoading(true);
    setOverviewLoading(true);
    setStatsError(null);
    setOverviewError(null);
    setRefreshKey((value) => value + 1);
  };

  return (
    <main className="usage-page" aria-labelledby="usage-title">
      <h1 className="sr-only" id="usage-title">
        Usage
      </h1>

      <div className="usage-toolbar">
        <div
          className="usage-period-toggle"
          role="group"
          aria-label="Usage period"
        >
          {ranges.map((value) => (
            <button
              type="button"
              aria-pressed={range === value}
              className={range === value ? "is-selected" : ""}
              onClick={() => selectRange(value)}
              key={value}
            >
              {value === "daily"
                ? "Today"
                : value === "weekly"
                  ? "7 days"
                  : "All time"}
            </button>
          ))}
        </div>
      </div>

      {error ? (
        <div className="usage-error" role="alert">
          <span>{error}</span>
          <button type="button" onClick={retry}>
            <RefreshCw aria-hidden="true" />
            Try again
          </button>
        </div>
      ) : null}

      {usageLoading ? (
        <section className="usage-summary is-loading" aria-live="polite">
          <span>Loading usage…</span>
        </section>
      ) : visibleUsage ? (
        <section className="usage-summary" aria-label={`${label} usage summary`}>
          <div className="usage-total">
            <span>Total tokens</span>
            <strong title={metricTitle(visibleUsage.totalTokens)}>
              {compactNumber.format(visibleUsage.totalTokens)}
            </strong>
            <small>{summaryDetail(range, visibleUsage, lifetime)}</small>
          </div>
          <dl className="usage-token-totals">
            <div>
              <dt>Fresh input</dt>
              <dd title={metricTitle(visibleUsage.promptTokens)}>
                {compactNumber.format(visibleUsage.promptTokens)}
              </dd>
            </div>
            <div>
              <dt>Cache read</dt>
              <dd title={metricTitle(visibleUsage.cacheReadTokens)}>
                {compactNumber.format(visibleUsage.cacheReadTokens)}
              </dd>
            </div>
            <div>
              <dt>Cache write</dt>
              <dd title={metricTitle(visibleUsage.cacheWriteTokens)}>
                {compactNumber.format(visibleUsage.cacheWriteTokens)}
              </dd>
              <small>
                {compactNumber.format(visibleUsage.cacheWriteOneHourTokens)} at
                1h
              </small>
            </div>
            <div>
              <dt>Output</dt>
              <dd title={metricTitle(visibleUsage.completionTokens)}>
                {compactNumber.format(visibleUsage.completionTokens)}
              </dd>
              <small>
                {compactNumber.format(visibleUsage.reasoningTokens)} reasoning
              </small>
            </div>
          </dl>
        </section>
      ) : null}

      <section className="usage-models-section" aria-labelledby="models-title">
        <header className="usage-content-heading">
          <h2 id="models-title">Models</h2>
          {visibleUsage ? (
            <span>
              {visibleUsage.modelsTruncated
                ? `${fullNumber.format(visibleUsage.models.length)}+ shown`
                : `${fullNumber.format(visibleUsage.models.length)} used`}
            </span>
          ) : null}
        </header>

        {usageLoading ? (
          <div className="usage-section-loading" aria-live="polite">
            Loading model usage…
          </div>
        ) : visibleUsage?.models.length ? (
          <div className="usage-model-table-scroll">
            <table className="usage-model-table">
              <caption className="sr-only">{label} usage by model</caption>
              <thead>
                <tr>
                  <th scope="col">Model</th>
                  <th scope="col">Requests</th>
                  <th scope="col">Fresh input</th>
                  <th scope="col">Cache read</th>
                  <th scope="col">Cache write</th>
                  <th scope="col">Output</th>
                  <th scope="col">Total</th>
                </tr>
              </thead>
              <tbody>
                {visibleUsage.models.map((model) => (
                  <tr key={`${model.provider}\0${model.model}`}>
                    <th scope="row">
                      <strong title={model.model}>{modelName(model.model)}</strong>
                      <small title={model.provider}>
                        {providerName(model.provider)}
                      </small>
                    </th>
                    <td title={metricTitle(model.requestCount)}>
                      {fullNumber.format(model.requestCount)}
                    </td>
                    <td title={metricTitle(model.promptTokens)}>
                      {compactNumber.format(model.promptTokens)}
                    </td>
                    <td title={metricTitle(model.cacheReadTokens)}>
                      {compactNumber.format(model.cacheReadTokens)}
                    </td>
                    <td title={metricTitle(model.cacheWriteTokens)}>
                      <strong>{compactNumber.format(model.cacheWriteTokens)}</strong>
                      <small>
                        {compactNumber.format(model.cacheWriteOneHourTokens)} at
                        1h
                      </small>
                    </td>
                    <td title={metricTitle(model.completionTokens)}>
                      <strong>{compactNumber.format(model.completionTokens)}</strong>
                      <small>
                        {compactNumber.format(model.reasoningTokens)} reasoning
                      </small>
                    </td>
                    <td title={metricTitle(model.totalTokens)}>
                      <strong>{compactNumber.format(model.totalTokens)}</strong>
                      <small>
                        {percentNumber.format(
                          visibleUsage.totalTokens === 0
                            ? 0
                            : model.totalTokens / visibleUsage.totalTokens,
                        )}
                      </small>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : visibleUsage ? (
          <p className="usage-model-empty">
            No completed model calls in this period.
          </p>
        ) : null}

        {visibleUsage?.modelsTruncated ? (
          <p className="usage-model-note">
            Showing the 256 models with the most tokens. Summary totals include
            all retained models.
          </p>
        ) : null}
      </section>

      <section className="usage-activity-section" aria-labelledby="activity-title">
        <header className="usage-content-heading">
          <h2 id="activity-title">Activity</h2>
          <span>Last 53 weeks</span>
        </header>
        {overviewLoading && !activity ? (
          <div className="usage-section-loading" aria-live="polite">
            Loading activity…
          </div>
        ) : activity ? (
          <TokenActivityHeatmap days={activity.days} />
        ) : null}
      </section>
    </main>
  );
}
