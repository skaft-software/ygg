import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Database,
  Flame,
  Gauge,
  History,
  RefreshCw,
  Sparkles,
} from "lucide-react";
import { useEffect, useState } from "react";
import type {
  LifetimeUsage,
  UsageActivity,
  UsagePeriod,
  UsageStats,
} from "../protocol";
import { TokenActivityHeatmap } from "../components/TokenActivityHeatmap";

const compactNumber = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});
const fullNumber = new Intl.NumberFormat();
const dateFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeZone: "UTC",
});

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
  const [period, setPeriod] = useState<UsagePeriod>("daily");
  const [stats, setStats] = useState<UsageStats | null>(null);
  const [lifetime, setLifetime] = useState<LifetimeUsage | null>(null);
  const [activity, setActivity] = useState<UsageActivity | null>(null);
  const [statsLoading, setStatsLoading] = useState(true);
  const [overviewLoading, setOverviewLoading] = useState(true);
  const [statsError, setStatsError] = useState<string | null>(null);
  const [overviewError, setOverviewError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    let cancelled = false;
    void loadStats(period)
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
  }, [loadStats, period, refreshKey]);

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

  const visibleStats = stats?.period === period ? stats : null;
  const error = statsError ?? overviewError;
  const selectPeriod = (nextPeriod: UsagePeriod) => {
    if (nextPeriod === period) return;
    setStatsLoading(true);
    setStatsError(null);
    setPeriod(nextPeriod);
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
      <header className="usage-hero">
        <span className="usage-hero-icon" aria-hidden="true">
          <Gauge />
        </span>
        <div>
          <span>Local inference telemetry</span>
          <h1 id="usage-title">Usage</h1>
          <p>
            Provider-reported token activity retained by this ygg host. Cache
            and reasoning tokens keep their original billing semantics.
          </p>
        </div>
      </header>

      {error ? (
        <div className="usage-error" role="alert">
          <span>{error}</span>
          <button type="button" onClick={retry}>
            <RefreshCw aria-hidden="true" />
            Try again
          </button>
        </div>
      ) : null}

      <section className="usage-period-section" aria-labelledby="period-title">
        <header className="usage-section-heading">
          <div>
            <span>Token breakdown</span>
            <h2 id="period-title">
              {period === "daily" ? "Today" : "Trailing seven days"}
            </h2>
          </div>
          <div
            className="usage-period-toggle"
            role="group"
            aria-label="Usage period"
          >
            {(["daily", "weekly"] as const).map((value) => (
              <button
                type="button"
                aria-pressed={period === value}
                className={period === value ? "is-selected" : ""}
                onClick={() => selectPeriod(value)}
                key={value}
              >
                {value === "daily" ? "Daily" : "Weekly"}
              </button>
            ))}
          </div>
        </header>

        {statsLoading && !visibleStats ? (
          <div className="usage-metric-grid is-loading" aria-live="polite">
            <span>Loading token totals…</span>
          </div>
        ) : visibleStats ? (
          <div className="usage-metric-grid">
            <article className="usage-metric is-total">
              <Sparkles aria-hidden="true" />
              <span>Total tokens</span>
              <strong title={metricTitle(visibleStats.totalTokens)}>
                {compactNumber.format(visibleStats.totalTokens)}
              </strong>
              <small>
                {fullNumber.format(visibleStats.requestCount)} completed
                {visibleStats.requestCount === 1 ? " request" : " requests"}
              </small>
            </article>
            <article className="usage-metric">
              <ArrowUpFromLine aria-hidden="true" />
              <span>Fresh input</span>
              <strong title={metricTitle(visibleStats.promptTokens)}>
                {compactNumber.format(visibleStats.promptTokens)}
              </strong>
              <small>Standard-rate prompt tokens</small>
            </article>
            <article className="usage-metric">
              <Database aria-hidden="true" />
              <span>Cache read</span>
              <strong title={metricTitle(visibleStats.cacheReadTokens)}>
                {compactNumber.format(visibleStats.cacheReadTokens)}
              </strong>
              <small>Prompt tokens served from cache</small>
            </article>
            <article className="usage-metric">
              <History aria-hidden="true" />
              <span>Cache write</span>
              <strong title={metricTitle(visibleStats.cacheWriteTokens)}>
                {compactNumber.format(visibleStats.cacheWriteTokens)}
              </strong>
              <small>
                {compactNumber.format(visibleStats.cacheWriteOneHourTokens)} at
                one-hour retention
              </small>
            </article>
            <article className="usage-metric">
              <ArrowDownToLine aria-hidden="true" />
              <span>Output</span>
              <strong title={metricTitle(visibleStats.completionTokens)}>
                {compactNumber.format(visibleStats.completionTokens)}
              </strong>
              <small>
                Includes {compactNumber.format(visibleStats.reasoningTokens)}
                reasoning tokens
              </small>
            </article>
          </div>
        ) : null}
      </section>

      <section className="usage-activity-section" aria-labelledby="activity-title">
        <header className="usage-section-heading">
          <div>
            <span>Last 53 weeks</span>
            <h2 id="activity-title">Token activity</h2>
          </div>
          {activity ? (
            <div className="usage-streaks" aria-label="Usage streaks">
              <span>
                <Flame aria-hidden="true" />
                <strong>{activity.currentStreak}</strong> current
              </span>
              <span>
                <strong>{activity.longestStreak}</strong> longest
              </span>
            </div>
          ) : null}
        </header>
        {overviewLoading && !activity ? (
          <div className="usage-activity-loading" aria-live="polite">
            Loading activity…
          </div>
        ) : activity ? (
          <TokenActivityHeatmap days={activity.days} />
        ) : null}
      </section>

      {lifetime ? (
        <section className="usage-lifetime" aria-labelledby="lifetime-title">
          <div>
            <span>All retained activity</span>
            <h2 id="lifetime-title">Lifetime</h2>
            <p>{lifetimeRange(lifetime)}</p>
          </div>
          <dl>
            <div>
              <dt>Tokens</dt>
              <dd title={metricTitle(lifetime.totalTokens)}>
                {compactNumber.format(lifetime.totalTokens)}
              </dd>
            </div>
            <div>
              <dt>Requests</dt>
              <dd>{fullNumber.format(lifetime.requestCount)}</dd>
            </div>
            <div>
              <dt>Cache read</dt>
              <dd title={metricTitle(lifetime.cacheReadTokens)}>
                {compactNumber.format(lifetime.cacheReadTokens)}
              </dd>
            </div>
          </dl>
        </section>
      ) : null}
    </main>
  );
}
