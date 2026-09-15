# mlxtop UX design rules

These rules apply to the terminal UI and to reviews of UI changes. Extend the
existing visual language before introducing a new convention.

## Naming and typography

| Element | Convention | Examples |
| --- | --- | --- |
| Navigation | Title case | Overview, MLX Top, Journal |
| Operational section heading | Uppercase | MODEL / STATE, THROUGHPUT, DIAGNOSIS |
| Chart title | Lowercase words; preserve acronyms | prompt load, generation, prefill, GPU |
| Status or compact metric label | Uppercase | LIVE, LAST SEEN, CACHE, HISTORY |
| Explanation or suggested action | Sentence case | Inspect added context or large tool results. |
| Keyboard hint | Match the actual key and use a short action | p pause, Home latest |

Charts at the same level must use the same title casing, weight, spacing and
border treatment. A chart does not become an operational section merely because
it spans the full width. Preserve standard units and names: tok/s, GiB, GPU,
oMLX and MLX. Use bold for the primary reading or status, not every line.

## Information hierarchy and space

- Overview answers: what is running, is it healthy, what is limiting it, and
  what should the operator inspect? Put current status and throughput first.
- Charts show trends and comparisons. Keep their primary reading, units and
  freshness adjacent. Keep supporting explanations shorter than the chart.
- On wide terminals, integrate prompt load into the chart grid at half width.
  Pair it with generation and prefill; group process memory and queue with
  supporting system metrics. Show a latency panel only when measured data exists.
- Use the smallest panel height that preserves readable values, labels and
  useful context. Extra terminal height should benefit the system traces;
  do not add empty rows to a request summary.
- Keep short-bar values readable independently of bar height. Do not put long
  request IDs or raw payloads in Overview; use Journal for request detail.
- At narrower widths, remove secondary detail before clipping primary values.
  Keep status, throughput, diagnosis and action available. Avoid wrapping chart
  headers or allowing long identifiers to displace readings.

## Color and emphasis

Use the existing palette; do not introduce per-widget color schemes.

| Role | Treatment |
| --- | --- |
| Healthy or favorable measured condition | Green plus a label or value |
| Condition worth inspecting | Yellow plus an explicit reason |
| Critical measured condition | Red plus an explicit reason |
| Chart identity | Existing metric color; a title color alone is not severity |
| Live request in prompt history | Cyan plus LIVE |
| Historical request in prompt history | Blue plus LAST SEEN or REPORTED and age |
| Material prompt increase | Yellow plus ! on the bar and a labeled comparison |
| Supporting text, unknown data, historical advice | Muted |

Never require color alone to interpret severity, freshness or selection. Use
text, symbols and values alongside it. A historical yellow bar records a past
comparison; it must not imply a current incident. Preserve captured severity
colors in time-series history instead of recoloring old samples on refresh.

## Metric truth and operator advice

- Keep implementation scope within mlxtop. Use existing provider interfaces
  and operating-system metrics; do not patch, inject into, reconfigure or
  restart serving runtimes to obtain counters. Omit unavailable-only dashboard
  summaries rather than filling operator space with implementation limitations.
- Unknown is `—`, not zero. Distinguish idle, stale, live and reported data.
- Show observation age for retained request values. Redrawing must not refresh
  their timestamps. Label historical assessments HISTORY.
- State chart units and scales. The prompt chart's 65,536-token ceiling is a
  display scale, not a model context limit; mark overflow and retain the exact
  selected value. Keep time-series gaps disconnected and one column per sample.
- Compare like-for-like provider/model observations. Say previous observed
  request; do not assume conversation membership or a tool loop.
- Show cache reuse only when reported for that request. Do not substitute an
  aggregate cache metric or infer latency from token counts alone.
- Stacked prompt bars use green for reported cached tokens, and cyan/live or
  blue/historical for the remainder. Unknown cache reuse stays unsplit. A yellow
  `!` and value label mark prompt growth without changing segment meanings.
- Label OS process memory with its PID and source. Keep process footprint,
  RSS and allocator counters distinct. Lifetime peak must not be confused with
  a peak observed only during monitoring. Growth requires consecutive readings
  of the same process instance; positive growth alone does not imply severity.
- Suggestions must follow visible evidence. Treat prompt-growth thresholds as
  comparison heuristics, not health alarms. Keep historical advice muted.

## Interaction and review

- Preserve the three views: Overview, MLX Top and Journal. Place request load
  in Overview, with contextual history controls and a clear selected marker.
- Keep navigation, pause, history and quit keys consistent. Show only controls
  relevant to the active view.
- Review at 80×24, a medium terminal, and a wide terminal. Check empty, live,
  idle/historical, unavailable and large-value states when affected.
- For layout changes, inspect an actual terminal rendering. For data semantics,
  use focused tests that verify freshness, comparisons and missing data. Follow
  the build and validation checks in [CONTRIBUTING.md](../CONTRIBUTING.md).
