// SPDX-License-Identifier: MIT
//! Compact operator view of sampled request load; never a billing ledger.
use super::*;

const HISTORY_LIMIT: usize = 240;
const TREND_CEILING: u64 = 65_536;

#[derive(Clone)]
struct Entry {
    number: u64,
    usage: providers::RequestUsage,
    last_seen: SystemTime,
}

#[derive(Clone, Default)]
pub(super) struct History {
    entries: VecDeque<Entry>,
    next_number: u64,
}

fn same_request(a: &providers::RequestUsage, b: &providers::RequestUsage) -> bool {
    a.id == b.id && a.provider == b.provider && a.model == b.model
}

impl History {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn observe(&mut self, requests: &[providers::RequestUsage]) {
        for usage in requests {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| same_request(&entry.usage, usage))
            {
                // Retained provider responses and repeated file reads do not
                // refresh an observation's age.
                entry.last_seen = usage.observed_at.unwrap_or(entry.last_seen);
                entry.usage = usage.clone();
            } else {
                self.next_number = self.next_number.saturating_add(1);
                self.entries.push_back(Entry {
                    number: self.next_number,
                    usage: usage.clone(),
                    last_seen: usage.observed_at.unwrap_or_else(SystemTime::now),
                });
                while self.entries.len() > HISTORY_LIMIT {
                    self.entries.pop_front();
                }
            }
        }
    }
}

fn exact(n: u64) -> String {
    let digits = n.to_string();
    let mut output = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            output.push(',');
        }
        output.push(c);
    }
    output
}

fn comparison(current: &providers::RequestUsage, previous: Option<&Entry>) -> String {
    let Some(previous) = previous else {
        return "PREVIOUS OBSERVED — awaiting another request".into();
    };
    let previous = &previous.usage;
    if current.provider != previous.provider || current.model != previous.model {
        return "PREVIOUS OBSERVED — different provider/model; no comparison".into();
    }
    let sign = if current.prompt >= previous.prompt {
        "+"
    } else {
        "−"
    };
    let amount = current.prompt.abs_diff(previous.prompt);
    let percent = if previous.prompt > 0 {
        format!(
            " ({sign}{:.1}%)",
            amount as f64 / previous.prompt as f64 * 100.0
        )
    } else {
        String::new()
    };
    format!(
        "PREVIOUS OBSERVED {} · CHANGE {sign}{}{percent}",
        exact(previous.prompt),
        exact(amount)
    )
}

fn chart_value(prompt: u64) -> u64 {
    prompt.min(TREND_CEILING)
}

fn comparable(a: &providers::RequestUsage, b: &providers::RequestUsage) -> bool {
    a.provider == b.provider && a.model == b.model
}

fn material_jump(current: &providers::RequestUsage, previous: Option<&Entry>) -> bool {
    previous.is_some_and(|old| {
        comparable(current, &old.usage)
            && current.prompt.saturating_sub(old.usage.prompt) >= 2048
            && old.usage.prompt > 0
            && current.prompt as u128 * 100 >= old.usage.prompt as u128 * 125
    })
}

struct Insight {
    title: String,
    detail: String,
    action: String,
    tone: Tone,
}

fn insight(history: &History, index: usize) -> Insight {
    let current = &history.entries[index].usage;
    let mut baseline: Vec<_> = history
        .entries
        .iter()
        .take(index)
        .rev()
        .take_while(|old| comparable(current, &old.usage))
        .take(8)
        .map(|old| old.usage.prompt)
        .collect();
    baseline.sort_unstable();
    let typical = if baseline.len() >= 3 {
        Some(baseline[baseline.len() / 2])
    } else {
        None
    };
    let detail = typical
        .map(|n| {
            format!(
                "Recent median {} · {} prior requests",
                exact(n),
                baseline.len()
            )
        })
        .unwrap_or_else(|| "Building a same-model baseline".into());
    if material_jump(
        current,
        index.checked_sub(1).and_then(|i| history.entries.get(i)),
    ) {
        return Insight {
            title: "PROMPT JUMP · input increased".into(),
            detail,
            action: "Inspect added context or large tool results.".into(),
            tone: Tone::Yellow,
        };
    }
    if let Some(median) = typical.filter(|median| *median > 0) {
        if current.prompt as u128 * 2 >= median as u128 * 3
            && current.prompt.saturating_sub(median) >= 2048
        {
            return Insight {
                title: format!(
                    "LARGE VS RECENT · {:.1}× median",
                    current.prompt as f64 / median as f64
                ),
                detail,
                action: "Inspect added context or large tool results.".into(),
                tone: Tone::Yellow,
            };
        }
        if current.prompt as u128 * 4 <= median as u128 * 3 {
            return Insight {
                title: "SMALLER INPUT · below recent median".into(),
                detail,
                action: "Less input; check prefill time for impact.".into(),
                tone: Tone::Cyan,
            };
        }
        return Insight {
            title: "TYPICAL INPUT · near recent median".into(),
            detail,
            action: "If slow, inspect prefill, queue and GPU.".into(),
            tone: Tone::Cyan,
        };
    }
    Insight {
        title: "BASELINE SAMPLING".into(),
        detail,
        action: "Compare more requests before judging size.".into(),
        tone: Tone::Muted,
    }
}

fn is_live(entry: &Entry, sample: &Sample, now: SystemTime) -> bool {
    sample.llm_source == TelemetrySource::Live
        && sample.llm_status != "stale"
        && !entry.usage.completed
        && now
            .duration_since(entry.last_seen)
            .is_ok_and(|age| age <= Duration::from_secs(5))
        && sample.llm_observed_at.is_some_and(|at| {
            now.duration_since(at)
                .is_ok_and(|age| age <= Duration::from_secs(5))
        })
        && sample
            .llm_requests
            .iter()
            .any(|request| same_request(&entry.usage, request))
}

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    sample: &Sample,
    scroll: usize,
) {
    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(
                " prompt load ",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "· 0–65,536 tokens · ↑↓ history / Home latest ",
                Style::default().fg(MUTED),
            ),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(" cached ", Style::default().fg(GREEN)),
            Span::styled("uncached ", Style::default().fg(CYAN)),
            Span::styled("history ", Style::default().fg(BLUE)),
            Span::styled("! jump ", Style::default().fg(YELLOW)),
            Span::styled("↑ overflow", Style::default().fg(MUTED)),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if history.entries.is_empty() {
        let text = if matches!(sample.llm_provider.as_str(), "oMLX" | "KoboldCpp") {
            "Waiting for per-request prompt counts."
        } else {
            "No per-request counts received. Connect client usage to see prompt load."
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    }
    let index = history.len() - 1 - scroll.min(history.len() - 1);
    let entry = &history.entries[index];
    let previous = index.checked_sub(1).and_then(|i| history.entries.get(i));
    let now = SystemTime::now();
    let live = is_live(entry, sample, now);
    let state = if live {
        "LIVE"
    } else if entry.usage.completed {
        "REPORTED"
    } else {
        "LAST SEEN"
    };
    let change = comparison(&entry.usage, previous);
    let header_height = if inner.width < 120 { 2 } else { 1 };
    let rows =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(1)]).split(inner);
    let headline = format!(
        "#{} · {} tokens · {state} · {}",
        entry.number,
        exact(entry.usage.prompt),
        telemetry_age(Some(entry.last_seen))
    );
    let summary = if header_height == 1 {
        vec![Line::from(vec![
            Span::styled(
                headline,
                Style::default()
                    .fg(if live { CYAN } else { BLUE })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   {change}"), Style::default().fg(MUTED)),
        ])]
    } else {
        vec![
            Line::from(Span::styled(
                headline,
                Style::default().fg(if live { CYAN } else { BLUE }),
            )),
            Line::from(Span::styled(change, Style::default().fg(MUTED))),
        ]
    };
    frame.render_widget(Paragraph::new(summary), rows[0]);
    let columns =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(rows[1]);
    let plot = columns[0];
    let count = usize::from(plot.width.saturating_sub(1) / 6).clamp(1, 32);
    let start = (index + 1).saturating_sub(count);
    // Values occupy their own row so short bars never hide their token count.
    let bar_area = Rect::new(
        plot.x,
        plot.y + 1,
        plot.width,
        plot.height.saturating_sub(1),
    );
    for (i, old) in history
        .entries
        .iter()
        .enumerate()
        .take(index + 1)
        .skip(start)
    {
        let jump = material_jump(
            &old.usage,
            i.checked_sub(1).and_then(|n| history.entries.get(n)),
        );
        let color = if is_live(old, sample, now) {
            CYAN
        } else {
            BLUE
        };
        let marker = if i == index && jump {
            "▶!"
        } else if i == index {
            "▶"
        } else if jump {
            "!"
        } else {
            "#"
        };
        let value = if old.usage.prompt > TREND_CEILING {
            "↑".into()
        } else {
            compact_tokens(old.usage.prompt)
        };
        let x = plot.x + ((i - start) * 6) as u16;
        if plot.height == 0 || x >= plot.right() {
            continue;
        }
        let width = 5.min(plot.right() - x);
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(if jump { YELLOW } else { color })),
            Rect::new(x, plot.y, width, 1),
        );
        if bar_area.height == 0 {
            continue;
        }
        frame.render_widget(
            Paragraph::new(format!("{marker}{}", old.number)).style(Style::default().fg(if jump {
                YELLOW
            } else {
                MUTED
            })),
            Rect::new(x, bar_area.bottom() - 1, width, 1),
        );
        let height = bar_area.height.saturating_sub(1);
        // Round total height to whole rows so a short stacked bar can show
        // both colors in one cell. Split that height at eighth-cell precision.
        let mut total = ((chart_value(old.usage.prompt) as u128 * height as u128)
            .div_ceil(TREND_CEILING as u128) as u64)
            * 8;
        if old
            .usage
            .cached
            .filter(|n| *n <= old.usage.prompt)
            .is_none()
        {
            total = (chart_value(old.usage.prompt) as u128 * height as u128 * 8
                / TREND_CEILING as u128) as u64;
        }
        let cached = old
            .usage
            .cached
            .filter(|n| *n <= old.usage.prompt)
            .filter(|_| old.usage.prompt > 0)
            .map(|n| (n as u128 * total as u128 / old.usage.prompt as u128) as u64)
            .unwrap_or(0)
            .min(total);
        for row in 0..height {
            let total_part = total.saturating_sub(u64::from(row) * 8).min(8) as usize;
            if total_part == 0 {
                continue;
            }
            let cached_part = cached.saturating_sub(u64::from(row) * 8).min(8) as usize;
            let blocks = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
            let (glyph, fg, bg) = if cached_part == total_part {
                (blocks[total_part], GREEN, PANEL)
            } else if cached_part > 0 && total_part == 8 {
                (blocks[cached_part], GREEN, color)
            } else {
                (blocks[total_part], color, PANEL)
            };
            for column in x..x + width {
                frame.buffer_mut()[(column, bar_area.bottom() - 2 - row)]
                    .set_symbol(glyph)
                    .set_fg(fg)
                    .set_bg(bg);
            }
        }
    }

    let mut assessment = insight(history, index);
    let cache = entry
        .usage
        .cached
        .filter(|n| *n <= entry.usage.prompt)
        .map(|cached| {
            let ratio = if entry.usage.prompt > 0 {
                cached as f64 / entry.usage.prompt as f64
            } else {
                0.0
            };
            if ratio < 0.2 && entry.usage.prompt >= 4096 {
                assessment.action = "For repeating prompts, check prefix reuse.".into();
            }
            let color = if ratio >= 0.8 {
                GREEN
            } else if ratio < 0.2 && entry.usage.prompt >= 4096 {
                YELLOW
            } else {
                CYAN
            };
            Span::styled(
                format!(
                    "CACHE {:.0}% · {} uncached",
                    ratio * 100.0,
                    exact(entry.usage.prompt - cached)
                ),
                Style::default().fg(color),
            )
        });
    if columns[1].width < 48 {
        assessment.title = assessment.title.split(" · ").next().unwrap_or("").into();
        assessment.detail = assessment.detail.split(" · ").next().unwrap_or("").into();
        assessment.action = if assessment.action.starts_with("Inspect added") {
            "Inspect context/tool results."
        } else if assessment.action.starts_with("For repeating") {
            "Repeating input? Check reuse."
        } else if assessment.action.starts_with("Less input") {
            "Compare prefill time."
        } else if assessment.action.starts_with("Compare more") {
            "Collect more requests."
        } else {
            "Check prefill, queue and GPU."
        }
        .into();
    }
    let title = if live {
        assessment.title
    } else {
        format!("HISTORY · {}", assessment.title)
    };
    let lines =
        vec![
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(assessment.tone.color())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(assessment.detail, Style::default().fg(MUTED))),
            Line::from(cache.unwrap_or_else(|| {
                Span::styled("CACHE — not reported", Style::default().fg(MUTED))
            })),
            Line::from(Span::styled(
                assessment.action,
                Style::default().fg(if live { Color::White } else { MUTED }),
            )),
        ];
    let fit: Vec<_> = lines
        .into_iter()
        .map(|line| {
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            Line::from(Span::styled(
                compact_label(&line.to_string(), columns[1].width as usize),
                style,
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(fit), columns[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn usage(id: &str, prompt: u64) -> providers::RequestUsage {
        providers::RequestUsage {
            provider: "oMLX".into(),
            model: "model".into(),
            id: id.into(),
            prompt,
            cached: None,
            output: None,
            completed: false,
            ttft_ms: None,
            observed_at: None,
        }
    }

    #[test]
    fn polls_update_one_bar_and_absent_requests_remain() {
        let mut history = History::default();
        history.observe(&[usage("a", 12000), usage("b", 20000)]);
        let mut updated = usage("a", 12000);
        updated.output = Some(80);
        history.observe(&[updated]);
        history.observe(&[]);
        assert_eq!(history.len(), 2);
        assert_eq!(history.entries[0].usage.output, Some(80));
        assert_eq!(history.entries[1].usage.prompt, 20000);
        assert_eq!(history.entries[0].number, 1);
    }

    #[test]
    fn history_is_bounded_and_ids_are_scoped_to_model_and_provider() {
        let mut history = History::default();
        let first = usage("a", 10);
        let mut second = first.clone();
        second.provider = "Ollama".into();
        let mut third = first.clone();
        third.model = "another".into();
        history.observe(&[first, second, third]);
        assert_eq!(history.len(), 3);
        for n in 0..300 {
            history.observe(&[usage(&n.to_string(), n)]);
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.entries.back().unwrap().number, 303);
    }
    #[test]
    fn comparison_is_explicit_and_handles_zero_and_provider_changes() {
        let mut history = History::default();
        history.observe(&[usage("previous", 22710)]);
        assert_eq!(
            comparison(&usage("latest", 20055), history.entries.back()),
            "PREVIOUS OBSERVED 22,710 · CHANGE −2,655 (−11.7%)"
        );
        let mut other = usage("other", 20055);
        other.model = "different".into();
        assert!(comparison(&other, history.entries.back()).contains("no comparison"));
        history.observe(&[usage("zero", 0)]);
        assert_eq!(
            comparison(&usage("new", 10), history.entries.back()),
            "PREVIOUS OBSERVED 0 · CHANGE +10"
        );
    }

    #[test]
    fn fixed_trend_scale_does_not_change_with_outliers() {
        let before = chart_value(20055);
        assert_eq!(chart_value(u64::MAX), TREND_CEILING);
        assert_eq!(chart_value(TREND_CEILING), TREND_CEILING);
        assert_eq!(chart_value(0), 0);
        assert_eq!(chart_value(20055), before);
    }

    #[test]
    fn live_requires_fresh_membership_and_file_history_keeps_its_age() {
        let now = SystemTime::now();
        let mut request = usage("active", 20055);
        request.observed_at = Some(now);
        let mut history = History::default();
        history.observe(&[request.clone()]);
        let mut sample = Sample {
            llm_source: TelemetrySource::Live,
            llm_status: "generating".into(),
            llm_observed_at: Some(now),
            llm_requests: vec![request.clone()],
            ..Sample::default()
        };
        let entry = history.entries.back().unwrap();
        assert!(is_live(entry, &sample, now));
        assert!(!is_live(entry, &sample, now + Duration::from_secs(6)));
        sample.llm_requests.clear();
        assert!(!is_live(entry, &sample, now));
        request.completed = true;
        request.observed_at = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000));
        history.observe(&[request.clone()]);
        history.observe(&[request]);
        assert_eq!(
            history.entries.back().unwrap().last_seen,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000)
        );
    }
    #[test]
    fn operator_insights_need_evidence_and_keep_model_boundaries() {
        let mut history = History::default();
        for (i, prompt) in [20000, 21000, 20500, 20055].into_iter().enumerate() {
            history.observe(&[usage(&i.to_string(), prompt)]);
        }
        assert!(insight(&history, 3).title.starts_with("TYPICAL INPUT"));
        history.observe(&[usage("jump", 40000)]);
        assert!(insight(&history, 4).title.starts_with("PROMPT JUMP"));
        assert_eq!(insight(&history, 4).tone, Tone::Yellow);
        let mut other = usage("other", 100000);
        other.model = "different".into();
        history.observe(&[other]);
        assert_eq!(insight(&history, 5).title, "BASELINE SAMPLING");
        assert!(!material_jump(
            &usage("small", 1000),
            Some(&history.entries[0])
        ));
    }

    #[test]
    fn short_prompt_bars_show_reported_cache_but_unknown_cache_stays_unsplit() {
        let mut history = History::default();
        let unknown = usage("unknown", 12000);
        let mut cached = usage("cached", 12000);
        cached.cached = Some(9000);
        history.observe(&[unknown, cached]);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 8)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &history, &Sample::default(), 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // The chart starts at x=1; the second bar begins six columns later.
        assert!((1..7).any(|y| buffer[(7, y)].fg == GREEN && buffer[(7, y)].bg == BLUE));
        assert!(!(1..7).any(|y| buffer[(1, y)].fg == GREEN));
    }

    #[test]
    fn colored_chart_and_operator_insights_render_together() {
        let now = SystemTime::now();
        let mut history = History::default();
        for (i, prompt) in [10000, 12000, 20000, 21000].into_iter().enumerate() {
            history.observe(&[usage(&i.to_string(), prompt)]);
        }
        let mut current = usage("live", 40000);
        current.cached = Some(36000);
        current.observed_at = Some(now);
        history.observe(&[current.clone()]);
        let sample = Sample {
            llm_source: TelemetrySource::Live,
            llm_status: "generating".into(),
            llm_observed_at: Some(now),
            llm_requests: vec![current],
            ..Sample::default()
        };
        let backend = ratatui::backend::TestBackend::new(180, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &history, &sample, 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for color in [BLUE, CYAN, YELLOW, GREEN] {
            assert!(
                buffer.content.iter().any(|cell| cell.fg == color),
                "missing semantic color {color:?}"
            );
        }
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        for label in [
            "LIVE",
            "10.0k",
            "12.0k",
            "PROMPT JUMP",
            "CACHE 90%",
            "Recent median",
            "Inspect added context",
        ] {
            assert!(text.contains(label), "missing operator insight: {label}");
        }
    }
}
