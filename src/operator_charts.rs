// SPDX-License-Identifier: MIT
//! Bounded OS/queue samples and explicitly reported request latency.
use super::*;

#[derive(Clone, Default)]
struct Point {
    active: Option<u64>,
    waiting: Option<u64>,
    footprint: Option<u64>,
    process: Option<(u32, u64)>,
    provider: String,
}

#[derive(Clone, Default)]
pub(super) struct History {
    points: VecDeque<Point>,
    timings: VecDeque<(String, Option<u64>, SystemTime)>,
}

impl History {
    pub fn observe(&mut self, sample: &Sample, limit: usize) {
        let fresh = sample.llm_source == TelemetrySource::Live
            && sample.llm_status != "stale"
            && sample.llm_observed_at.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age <= Duration::from_secs(5))
            });
        self.points.push_back(Point {
            active: fresh.then_some(sample.llm_active_requests).flatten(),
            waiting: fresh.then_some(sample.llm_waiting_requests).flatten(),
            footprint: sample.process_memory.as_ref().map(|m| m.footprint),
            process: sample.process_memory.as_ref().map(|m| (m.pid, m.started)),
            provider: sample.llm_provider.clone(),
        });
        while self.points.len() > limit {
            self.points.pop_front();
        }
        for request in &sample.llm_requests {
            let key = format!("{}\0{}\0{}", request.provider, request.model, request.id);
            if let Some(old) = self.timings.iter_mut().find(|old| old.0 == key) {
                if let Some(ttft) = request.ttft_ms {
                    old.1 = Some(ttft);
                    old.2 = request.observed_at.unwrap_or(old.2);
                }
            } else {
                self.timings.push_back((
                    key,
                    request.ttft_ms,
                    request.observed_at.unwrap_or_else(SystemTime::now),
                ));
            }
        }
        while self.timings.len() > 240 {
            self.timings.pop_front();
        }
    }

    pub fn has_latency(&self) -> bool {
        self.timings.iter().any(|p| p.1.is_some())
    }
}

// One column per sample, fixed scale, no connections across missing readings
// or changed process/provider identity. Clipping never changes the scale.
fn trace(
    frame: &mut Frame,
    area: Rect,
    values: &[Option<u64>],
    breaks: &[bool],
    ceiling: u64,
    color: Color,
) {
    if area.is_empty() || ceiling == 0 {
        return;
    }
    let start = values.len().saturating_sub(area.width as usize);
    let offset = area.width as usize - (values.len() - start);
    let mut previous: Option<u16> = None;
    for (i, value) in values.iter().enumerate().skip(start) {
        if breaks.get(i).copied().unwrap_or(false) {
            previous = None;
        }
        let Some(value) = value else {
            previous = None;
            continue;
        };
        let x = area.x + (offset + i - start) as u16;
        let scaled = (u128::from((*value).min(ceiling)) * u128::from(area.height - 1))
            .div_ceil(u128::from(ceiling)) as u16;
        let y = area.bottom() - 1 - scaled;
        let mut glyph = "━";
        if let Some(old_y) = previous.filter(|old_y| *old_y != y) {
            for row in old_y.min(y) + 1..old_y.max(y) {
                frame.buffer_mut()[(x, row)].set_symbol("┃").set_fg(color);
            }
            frame.buffer_mut()[(x, old_y)]
                .set_symbol(if y > old_y { "┓" } else { "┛" })
                .set_fg(color);
            glyph = if y > old_y { "┗" } else { "┏" };
        }
        frame.buffer_mut()[(x, y)]
            .set_symbol(if *value > ceiling { "↑" } else { glyph })
            .set_fg(color);
        previous = Some(y);
    }
}

fn panel_area(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    caption: String,
    subtitle: String,
) -> Rect {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .style(Style::default().bg(PANEL))
        .title(Line::from(vec![
            Span::styled(
                format!(" {name} "),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(caption, Style::default().fg(MUTED)),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return inner;
    }
    frame.render_widget(
        Paragraph::new(subtitle).style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    )
}

pub(super) fn queue(frame: &mut Frame, area: Rect, history: &History, interval: Duration) {
    let last = history.points.back();
    let active = last.and_then(|p| p.active);
    let waiting = last.and_then(|p| p.waiting);
    let plot = panel_area(
        frame,
        area,
        "queue",
        format!("active {} · waiting {} ", count(active), count(waiting)),
        format!(
            "0–16 req · ↑ overflow · {}",
            chart_window_label(
                history
                    .points
                    .len()
                    .min(area.width.saturating_sub(5) as usize),
                interval
            )
        ),
    );
    let active_values: Vec<_> = history.points.iter().map(|p| p.active).collect();
    let waiting_values: Vec<_> = history.points.iter().map(|p| p.waiting).collect();
    let breaks: Vec<_> = history
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| i > 0 && p.provider != history.points[i - 1].provider)
        .collect();
    if plot.height < 2 || plot.width < 4 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("active ", Style::default().fg(CYAN)),
            Span::styled("waiting ", Style::default().fg(YELLOW)),
            Span::styled("═ overlap", Style::default().fg(Color::White)),
        ])),
        Rect::new(plot.x, plot.y, plot.width, 1),
    );
    let graph = Rect::new(plot.x + 3, plot.y + 1, plot.width - 3, plot.height - 1);
    for (y, label) in [(graph.y, "16"), (graph.bottom() - 1, "0")] {
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(MUTED)),
            Rect::new(plot.x, y, 3, 1),
        );
    }
    for x in graph.x..graph.right() {
        frame.buffer_mut()[(x, graph.bottom() - 1)]
            .set_symbol("─")
            .set_fg(DIM);
    }
    trace(frame, graph, &active_values, &breaks, 16, CYAN);
    let active_cells: Vec<_> = (graph.y..graph.bottom())
        .flat_map(|y| (graph.x..graph.right()).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            let cell = &frame.buffer_mut()[(x, y)];
            (cell.fg == CYAN).then_some((x, y, cell.symbol() == "↑"))
        })
        .collect();
    trace(frame, graph, &waiting_values, &breaks, 16, YELLOW);
    // Preserve both series wherever their rasterized traces share a cell.
    // This includes equal values and values indistinguishable at terminal resolution.
    for (x, y, active_overflow) in active_cells {
        if frame.buffer_mut()[(x, y)].fg == YELLOW {
            let overflow = active_overflow || frame.buffer_mut()[(x, y)].symbol() == "↑";
            frame.buffer_mut()[(x, y)]
                .set_symbol(if overflow { "↑" } else { "═" })
                .set_fg(Color::White);
        }
    }
    if active_values
        .iter()
        .chain(&waiting_values)
        .all(Option::is_none)
    {
        frame.render_widget(
            Paragraph::new("No queue samples").style(Style::default().fg(MUTED)),
            graph,
        );
    }
}

pub(super) fn footprint(frame: &mut Frame, area: Rect, history: &History, sample: &Sample) {
    let last = sample.process_memory.as_ref();
    let caption = last
        .map(|m| format!("{} · PID {} ", bytes(m.footprint), m.pid))
        .unwrap_or_else(|| "— ".into());
    let ceiling = sample.total_memory;
    let subtitle = if ceiling > 0 {
        format!("OS · 0–{} RAM · ↑ overflow", bytes(ceiling))
    } else {
        "OS · scale unavailable".into()
    };
    let plot = panel_area(frame, area, "process memory", caption, subtitle);
    let values: Vec<_> = history.points.iter().map(|p| p.footprint).collect();
    let breaks: Vec<_> = history
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| i > 0 && p.process != history.points[i - 1].process)
        .collect();
    trace(frame, plot, &values, &breaks, ceiling, CYAN);
}

pub(super) fn latency(frame: &mut Frame, area: Rect, history: &History) {
    let latest = history.timings.iter().rev().find(|p| p.1.is_some());
    let caption = latest
        .map(|p| format!("{} ms · REPORTED ", p.1.unwrap()))
        .unwrap_or_else(|| "— ".into());
    let subtitle = latest
        .map(|p| {
            format!(
                "0–30s · one column/request · ↑ overflow · {}",
                telemetry_age(Some(p.2))
            )
        })
        .unwrap_or_default();
    let plot = panel_area(frame, area, "first token", caption, subtitle);
    let values: Vec<_> = history.timings.iter().map(|p| p.1).collect();
    // Each column is one observed request; do not connect unrelated requests.
    for (i, value) in values.iter().rev().take(plot.width as usize).enumerate() {
        if let Some(value) = value {
            let x = plot.right().saturating_sub(1 + i as u16);
            let h = if plot.height == 0 {
                0
            } else {
                (((*value).min(30_000) as u128 * plot.height as u128).div_ceil(30_000) as u16)
                    .max(1)
            };
            for y in plot.bottom().saturating_sub(h)..plot.bottom() {
                frame.buffer_mut()[(x, y)]
                    .set_symbol(if *value > 30_000 { "↑" } else { "▇" })
                    .set_fg(BLUE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> Sample {
        Sample {
            llm_source: TelemetrySource::Live,
            llm_status: "idle".into(),
            llm_provider: "oMLX".into(),
            llm_observed_at: Some(SystemTime::now()),
            llm_active_requests: Some(0),
            llm_waiting_requests: Some(0),
            ..Sample::default()
        }
    }

    #[test]
    fn queue_keeps_idle_zero_but_gaps_stale_and_reported_values() {
        let mut history = History::default();
        let mut sample = live();
        history.observe(&sample, 10);
        assert_eq!(history.points.back().unwrap().active, Some(0));
        sample.llm_observed_at = Some(SystemTime::now() - Duration::from_secs(20));
        history.observe(&sample, 10);
        assert_eq!(history.points.back().unwrap().active, None);
        sample.llm_observed_at = Some(SystemTime::now());
        sample.llm_source = TelemetrySource::Report;
        history.observe(&sample, 10);
        assert_eq!(history.points.back().unwrap().waiting, None);
        sample.llm_source = TelemetrySource::Live;
        sample.llm_waiting_requests = None;
        history.observe(&sample, 2);
        assert_eq!(history.points.len(), 2);
        assert_eq!(history.points.back().unwrap().waiting, None);
    }

    #[test]
    fn latency_requires_explicit_measurement_and_deduplicates_requests() {
        let mut sample = live();
        sample.llm_requests.push(providers::RequestUsage {
            provider: "oMLX".into(),
            model: "test".into(),
            id: "one".into(),
            prompt: 100,
            cached: None,
            output: None,
            completed: true,
            observed_at: Some(SystemTime::now()),
            ttft_ms: None,
        });
        let mut history = History::default();
        history.observe(&sample, 10);
        assert!(!history.has_latency());
        sample.llm_requests[0].ttft_ms = Some(0);
        history.observe(&sample, 10);
        history.observe(&sample, 10);
        assert!(history.has_latency());
        assert_eq!(history.timings.len(), 1);
        sample.llm_requests[0].model = "other".into();
        history.observe(&sample, 10);
        assert_eq!(history.timings.len(), 2);
    }

    #[test]
    fn stepped_trace_uses_connected_corners_for_rises_and_falls() {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(3, 5)).unwrap();
        terminal
            .draw(|frame| {
                trace(
                    frame,
                    frame.area(),
                    &[Some(0), Some(16), Some(0)],
                    &[false; 3],
                    16,
                    CYAN,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 4)].symbol(), "┛");
        assert_eq!(buffer[(1, 0)].symbol(), "┏");
        assert_eq!(buffer[(2, 0)].symbol(), "┓");
        assert_eq!(buffer[(2, 4)].symbol(), "┗");
    }

    #[test]
    fn zero_queue_series_share_labeled_baseline_and_keep_border_intact() {
        let mut history = History::default();
        for _ in 0..40 {
            history.observe(&live(), 80);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(44, 10)).unwrap();
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(40, 8)].symbol(), "═");
        assert_eq!(buffer[(40, 8)].fg, Color::White);
        assert_eq!(buffer[(1, 8)].symbol(), "0");
        assert!((3..8).all(|y| buffer[(40, y)].symbol() == " "));
        assert_eq!(buffer[(1, 9)].symbol(), "─");
    }

    #[test]
    fn unequal_queue_counts_use_one_scale_and_missing_series_is_not_overlap() {
        let mut history = History::default();
        let mut sample = live();
        sample.llm_active_requests = Some(8);
        for _ in 0..40 {
            history.observe(&sample, 80);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(44, 10)).unwrap();
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(40, 5)].fg, CYAN);
        assert_eq!(buffer[(40, 8)].fg, YELLOW);
        history = History::default();
        sample.llm_active_requests = None;
        for _ in 0..40 {
            history.observe(&sample, 80);
        }
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1)))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(40, 8)].symbol(), "━");
        assert_eq!(terminal.backend().buffer()[(40, 8)].fg, YELLOW);
    }

    #[test]
    fn trace_preserves_gaps_boundaries_and_overflow() {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(5, 5)).unwrap();
        terminal
            .draw(|frame| {
                trace(
                    frame,
                    frame.area(),
                    &[Some(0), None, Some(16), Some(0), Some(u64::MAX)],
                    &[false, false, false, true, false],
                    16,
                    CYAN,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 2)].symbol(), " ");
        assert_eq!(buffer[(3, 2)].symbol(), " ");
        assert_eq!(buffer[(4, 0)].symbol(), "↑");
    }
}
