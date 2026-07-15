import { useI18n } from "~/i18n";

type BenchmarkPoint = {
  depth: number;
  seconds: number | null;
};

const MYSQL: BenchmarkPoint[] = [
  { depth: 2, seconds: 0.016 },
  { depth: 3, seconds: 30.267 },
  { depth: 4, seconds: 1543.505 },
  { depth: 5, seconds: null },
];

const NEO4J: BenchmarkPoint[] = [
  { depth: 2, seconds: 0.01 },
  { depth: 3, seconds: 0.168 },
  { depth: 4, seconds: 1.359 },
  { depth: 5, seconds: 2.132 },
];

const WIDTH = 520;
const HEIGHT = 250;
const LEFT = 58;
const RIGHT = 20;
const TOP = 24;
const BOTTOM = 46;
const PLOT_WIDTH = WIDTH - LEFT - RIGHT;
const PLOT_HEIGHT = HEIGHT - TOP - BOTTOM;
const MIN_SECONDS = 0.01;
const MAX_SECONDS = 3600;
const Y_TICKS = [0.01, 0.1, 1, 10, 100, 1000, 3600];

const xForDepth = (depth: number): number => LEFT + ((depth - 2) / 3) * PLOT_WIDTH;

const yForSeconds = (seconds: number): number => {
  const minLog = Math.log10(MIN_SECONDS);
  const maxLog = Math.log10(MAX_SECONDS);
  return TOP + (1 - (Math.log10(seconds) - minLog) / (maxLog - minLog)) * PLOT_HEIGHT;
};

const formatSeconds = (seconds: number): string => {
  if (seconds < 1) return `${seconds.toFixed(3)}s`;
  if (seconds >= 1000) return `${seconds.toLocaleString("en-US", { maximumFractionDigits: 3 })}s`;
  return `${seconds.toFixed(3)}s`;
};

const linePoints = (points: BenchmarkPoint[]): string =>
  points
    .filter((point): point is BenchmarkPoint & { seconds: number } => point.seconds !== null)
    .map((point) => `${xForDepth(point.depth)},${yForSeconds(point.seconds)}`)
    .join(" ");

export function TopicPathPerformanceChart() {
  const { t } = useI18n();

  return (
    <figure class="mt-4 rounded-lg border border-slate-200 bg-slate-50 p-3">
      <figcaption class="mb-2 text-sm font-semibold text-slate-700">
        {t("explanation.topicPathChartTitle")}
      </figcaption>
      <svg
        class="h-auto w-full"
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        role="img"
        aria-labelledby="topic-path-chart-title topic-path-chart-description"
      >
        <title id="topic-path-chart-title">{t("explanation.topicPathChartTitle")}</title>
        <desc id="topic-path-chart-description">{t("explanation.topicPathChartDescription")}</desc>

        {Y_TICKS.map((tick) => {
          const y = yForSeconds(tick);
          return (
            <g>
              <line x1={LEFT} x2={WIDTH - RIGHT} y1={y} y2={y} stroke="#cbd5e1" stroke-dasharray="3 3" />
              <text x={LEFT - 8} y={y + 4} text-anchor="end" class="fill-slate-500 text-[10px]">
                {tick === 3600 ? "1h" : tick < 1 ? `${tick}s` : `${tick}s`}
              </text>
            </g>
          );
        })}

        <line x1={LEFT} x2={LEFT} y1={TOP} y2={HEIGHT - BOTTOM} stroke="#64748b" />
        <line x1={LEFT} x2={WIDTH - RIGHT} y1={HEIGHT - BOTTOM} y2={HEIGHT - BOTTOM} stroke="#64748b" />

        {[2, 3, 4, 5].map((depth) => (
          <g>
            <line
              x1={xForDepth(depth)}
              x2={xForDepth(depth)}
              y1={HEIGHT - BOTTOM}
              y2={HEIGHT - BOTTOM + 5}
              stroke="#64748b"
            />
            <text x={xForDepth(depth)} y={HEIGHT - BOTTOM + 19} text-anchor="middle" class="fill-slate-600 text-[11px]">
              {depth}
            </text>
          </g>
        ))}

        <polyline fill="none" stroke="#e11d48" stroke-width="3" points={linePoints(MYSQL)} />
        <polyline fill="none" stroke="#4f46e5" stroke-width="3" points={linePoints(NEO4J)} />

        {MYSQL.map((point) =>
          point.seconds === null ? null : (
            <circle cx={xForDepth(point.depth)} cy={yForSeconds(point.seconds)} r="4" fill="#e11d48" />
          ),
        )}
        {NEO4J.map((point) =>
          point.seconds === null ? null : (
            <circle cx={xForDepth(point.depth)} cy={yForSeconds(point.seconds)} r="4" fill="#4f46e5" />
          ),
        )}

        <path
          d={`M ${xForDepth(5)} ${TOP - 1} l -5 8 h 10 z`}
          fill="#e11d48"
          aria-label={t("explanation.topicPathChartMysqlIncomplete")}
        />
        <text x={xForDepth(5) - 8} y={TOP - 5} text-anchor="end" class="fill-rose-700 text-[10px]">
          {t("explanation.topicPathChartMysqlIncomplete")}
        </text>

        <text x={LEFT} y={HEIGHT - 8} class="fill-slate-500 text-[10px]">
          {t("explanation.topicPathChartDepth")}
        </text>
        <text x={LEFT} y={TOP - 8} class="fill-slate-500 text-[10px]">
          {t("explanation.topicPathChartLogScale")}
        </text>
      </svg>
      <div class="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-slate-600" aria-hidden="true">
        <span><span class="mr-1 inline-block h-2 w-2 rounded-full bg-rose-600" />{t("explanation.topicPathChartMysql")}</span>
        <span><span class="mr-1 inline-block h-2 w-2 rounded-full bg-indigo-600" />{t("explanation.topicPathChartNeo4j")}</span>
      </div>
      <p class="mt-2 text-xs leading-relaxed text-slate-500">{t("explanation.topicPathChartCaption")}</p>
    </figure>
  );
}
