// SPDX-License-Identifier: MIT
//! Read-only provider adapters. Only counters and identifiers cross into samples.
use super::*;

const MAX_USAGE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub(super) struct RequestUsage {
    pub provider: String,
    pub model: String,
    pub id: String,
    pub prompt: u64,
    pub cached: Option<u64>,
    pub output: Option<u64>,
    pub completed: bool,
    pub ttft_ms: Option<u64>,
    pub observed_at: Option<SystemTime>,
}

impl RequestUsage {
    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} prompt {} · out {} · {} · {} · {}",
            if self.completed {
                "reported"
            } else {
                "observed"
            },
            self.prompt,
            optional_tokens(self.output),
            self.provider,
            self.model,
            self.id
        );
        if let Some(cached) = self.cached {
            text.push_str(&format!(" · cached {cached}"));
        }
        if let Some(ttft) = self.ttft_ms {
            text.push_str(&format!(" · first token {ttft} ms (reported)"));
        }
        text
    }
}

pub(super) fn new_request_summary(
    seen: &mut VecDeque<(String, u64)>,
    request: &RequestUsage,
) -> Option<String> {
    let key = format!("{}\0{}\0{}", request.provider, request.model, request.id);
    if seen
        .iter()
        .any(|entry| entry == &(key.clone(), request.prompt))
    {
        return None;
    }
    seen.push_back((key, request.prompt));
    while seen.len() > 512 {
        seen.pop_front();
    }
    Some(request.summary())
}

fn counter(value: &Value, path: &[&str]) -> Option<u64> {
    json_value(value, path)?.as_u64()
}

fn identifier(value: &Value, field: &str) -> Option<String> {
    let value = value.get(field)?;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    let text: String = text.chars().filter(|c| !c.is_control()).take(120).collect();
    (!text.is_empty()).then_some(text)
}

pub(super) fn omlx_requests(stats: &Value) -> Vec<RequestUsage> {
    let Some(models) = stats
        .pointer("/active_models/models")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for model in models {
        for phase in ["generating", "prefilling", "waiting"] {
            let Some(requests) = model.get(phase).and_then(Value::as_array) else {
                continue;
            };
            for request in requests {
                let (Some(id), Some(prompt)) = (
                    identifier(request, "request_id"),
                    counter(request, &["prompt_tokens"]),
                ) else {
                    continue;
                };
                // Waiting requests may not have been tokenized yet.
                if prompt == 0 && phase == "waiting" {
                    continue;
                }
                result.push(RequestUsage {
                    provider: "oMLX".into(),
                    model: identifier(model, "id").unwrap_or_else(|| "unknown".into()),
                    id,
                    prompt,
                    cached: counter(request, &["cached_tokens"]).filter(|n| *n <= prompt),
                    output: counter(request, &["generated_tokens"]),
                    completed: false,
                    ttft_ms: None,
                    observed_at: Some(SystemTime::now()),
                });
            }
        }
    }
    result
}

fn canonical_provider(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "omlx" => Some("oMLX"),
        "mlx-lm" | "mlx_lm.server" | "mlx_lm" => Some("mlx-lm"),
        "ollama" => Some("Ollama"),
        "llama.cpp" | "llama-server" => Some("llama.cpp"),
        "lm studio" | "lmstudio" => Some("LM Studio"),
        "koboldcpp" => Some("KoboldCpp"),
        "localai" => Some("LocalAI"),
        _ => None,
    }
}

pub(super) struct Adapter {
    configured: Option<String>,
    usage_file: Option<PathBuf>,
    selected: Option<String>,
    port: Option<u16>,
    cached: Option<LlmTelemetry>,
    next_poll: Instant,
    backoff: Duration,
    kobold_uptime: Option<f64>,
    kobold_session: u64,
}

impl Adapter {
    pub fn new() -> Self {
        Self {
            configured: env::var("MLXTOP_PROVIDER").ok(),
            usage_file: env::var_os("MLXTOP_USAGE_FILE").map(PathBuf::from),
            selected: None,
            port: env::var("MLXTOP_PROVIDER_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|p| *p > 0),
            cached: None,
            next_poll: Instant::now(),
            backoff: Duration::from_secs(1),
            kobold_uptime: None,
            kobold_session: 0,
        }
    }

    pub fn selected(&mut self, detected: Option<&str>) -> bool {
        let selected = self
            .configured
            .as_deref()
            .or(detected)
            .map(|s| canonical_provider(s).unwrap_or("unsupported").to_owned());
        if selected != self.selected {
            self.selected = selected;
            self.cached = None;
            self.next_poll = Instant::now();
            self.backoff = Duration::from_secs(1);
            self.kobold_uptime = None;
            self.kobold_session = self.kobold_session.saturating_add(1);
        }
        self.usage_file.is_some() || self.selected.as_deref().is_some_and(|s| s != "oMLX")
    }

    pub fn poll(&mut self) -> Option<LlmTelemetry> {
        let now = Instant::now();
        if now < self.next_poll {
            return self.with_usage();
        }
        let result = match self.selected.as_deref() {
            Some("KoboldCpp") => self.poll_kobold(),
            Some("llama.cpp") => self.poll_llama(),
            _ => None,
        };
        if let Some(mut result) = result {
            // A successful poll does not make KoboldCpp's last completion new.
            if result.provider.as_deref() == Some("KoboldCpp") {
                if let Some(previous) = &self.cached {
                    if result.requests.first().map(|r| &r.id)
                        == previous.requests.first().map(|r| &r.id)
                    {
                        result.observed_at = previous.observed_at;
                    }
                }
            }
            self.cached = Some(result);
            self.backoff = Duration::from_secs(1);
        } else {
            self.backoff = (self.backoff * 2).min(Duration::from_secs(30));
        }
        self.next_poll = now + self.backoff;
        self.with_usage()
    }

    fn with_usage(&self) -> Option<LlmTelemetry> {
        let reported = self
            .usage_file
            .as_ref()
            .and_then(|path| read_usage_file(path, self.selected.as_deref()));
        match (self.cached.clone(), reported) {
            (Some(mut native), Some(reported)) if native.source == TelemetrySource::Live => {
                // Completed prompt counts belong in request history. Never attach
                // them to the output/queue of an unrelated active request.
                native.requests.extend(reported.requests);
                Some(native)
            }
            (native, reported) => reported.or(native),
        }
    }

    fn get(&self, port: u16, path: &str) -> Option<String> {
        let response = http_request(
            "127.0.0.1",
            self.port.unwrap_or(port),
            "GET",
            path,
            &[],
            None,
        )?;
        (response.status == 200).then_some(response.body)
    }

    fn poll_kobold(&mut self) -> Option<LlmTelemetry> {
        let perf: Value = serde_json::from_str(&self.get(5001, "/api/extra/perf")?).ok()?;
        self.kobold_result(&perf)
    }

    fn kobold_result(&mut self, perf: &Value) -> Option<LlmTelemetry> {
        let mut result = parse_kobold(perf)?;
        let uptime = perf
            .get("uptime")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0);
        if uptime
            .zip(self.kobold_uptime)
            .is_some_and(|(now, old)| now < old)
        {
            self.kobold_session = self.kobold_session.saturating_add(1);
        }
        self.kobold_uptime = uptime.or(self.kobold_uptime);
        for request in &mut result.requests {
            request.id = format!("session-{}-{}", self.kobold_session, request.id);
        }
        Some(result)
    }

    fn poll_llama(&self) -> Option<LlmTelemetry> {
        // Either monitoring endpoint may be disabled independently.
        let slots = self
            .get(8080, "/slots")
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .and_then(|slots| parse_llama_slots(&slots));
        let metrics = self.get(8080, "/metrics");
        merge_llama_metrics(slots, metrics.as_deref())
    }
}

fn merge_llama_metrics(slots: Option<LlmTelemetry>, metrics: Option<&str>) -> Option<LlmTelemetry> {
    let Some(metrics) = metrics else {
        return slots;
    };
    let generation = metric(metrics, "llamacpp:predicted_tokens_seconds");
    let prefill = metric(metrics, "llamacpp:prompt_tokens_seconds");
    let active = metric_count(metrics, "llamacpp:requests_processing");
    let waiting = metric_count(metrics, "llamacpp:requests_deferred");
    if generation.is_none() && prefill.is_none() && active.is_none() && waiting.is_none() {
        return slots;
    }
    let mut result = slots.unwrap_or_else(|| LlmTelemetry {
        source: TelemetrySource::Live,
        observed_at: Some(SystemTime::now()),
        provider: Some("llama.cpp".into()),
        ..LlmTelemetry::default()
    });
    result.generation_tps = generation;
    result.prefill_tps = prefill;
    result.active_requests = result.active_requests.or(active);
    result.waiting_requests = waiting;
    result.status = Some(
        match result.active_requests {
            Some(0) if waiting.unwrap_or(0) == 0 => "idle",
            Some(_) => "processing",
            None => "running",
        }
        .into(),
    );
    Some(result)
}

fn metric_count(text: &str, name: &str) -> Option<u64> {
    metric(text, name)
        .filter(|n| n.fract() == 0.0 && *n < u64::MAX as f64)
        .map(|n| n as u64)
}

fn metric(text: &str, name: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let key = fields.next()?;
        if key != name {
            return None;
        }
        fields
            .next()?
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    })
}

fn parse_llama_slots(slots: &Value) -> Option<LlmTelemetry> {
    let slots = slots.as_array()?;
    if slots
        .iter()
        .any(|slot| slot.get("is_processing").and_then(Value::as_bool).is_none())
    {
        return None;
    }
    let active: Vec<_> = slots
        .iter()
        .filter(|s| s["is_processing"] == true)
        .collect();
    Some(LlmTelemetry {
        source: TelemetrySource::Live,
        observed_at: Some(SystemTime::now()),
        provider: Some("llama.cpp".into()),
        status: Some(
            if active.is_empty() {
                "idle"
            } else {
                "processing"
            }
            .into(),
        ),
        active_requests: Some(active.len() as u64),
        // Sum every active slot, but never present a partial sum as complete.
        output_tokens: (!active.is_empty())
            .then(|| {
                active.iter().try_fold(0_u64, |sum, slot| {
                    sum.checked_add(counter(slot, &["next_token", "n_decoded"])?)
                })
            })
            .flatten(),
        // n_ctx is capacity, n_decoded is output, and prompt_n is fresh prefill
        // work. None is a substitute for a full per-request prompt count.
        ..LlmTelemetry::default()
    })
}

fn parse_kobold(perf: &Value) -> Option<LlmTelemetry> {
    let generations = counter(perf, &["total_gens"])?;
    let prompt = (generations > 0)
        .then(|| counter(perf, &["last_input_count"]))
        .flatten();
    let output = (generations > 0)
        .then(|| counter(perf, &["last_token_count"]))
        .flatten();
    // The endpoint describes the last result, even when another request runs.
    let requests = prompt
        .map(|prompt| RequestUsage {
            provider: "KoboldCpp".into(),
            model: "unknown".into(),
            id: format!("generation-{generations}"),
            prompt,
            cached: None,
            output,
            completed: true,
            ttft_ms: None,
            observed_at: None,
        })
        .into_iter()
        .collect();
    Some(LlmTelemetry {
        source: TelemetrySource::Report,
        observed_at: Some(SystemTime::now()),
        provider: Some("KoboldCpp".into()),
        status: Some(
            if generations > 0 {
                "last result"
            } else {
                "idle"
            }
            .into(),
        ),
        prompt_tokens: prompt,
        output_tokens: output,
        requests,
        generation_tps: (generations > 0)
            .then(|| request_rate(perf, &["last_eval_speed"]))
            .flatten(),
        prefill_tps: (generations > 0)
            .then(|| request_rate(perf, &["last_process_speed"]))
            .flatten(),
        ..LlmTelemetry::default()
    })
}

fn parse_usage(record: &Value) -> Option<LlmTelemetry> {
    if record.get("done").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let provider = canonical_provider(record.get("provider")?.as_str()?)?;
    let id = identifier(record, "request_id")?;
    let timestamp = counter(record, &["observed_at"])?;
    let observed_at = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?;
    if observed_at > SystemTime::now() {
        return None;
    }
    let usage = record
        .get("usage")
        .or_else(|| record.get("stats"))
        .unwrap_or(record);
    let prompt = counter(usage, &["prompt_tokens"])
        .or_else(|| counter(usage, &["input_tokens"]))
        .or_else(|| counter(usage, &["prompt_eval_count"]))?;
    let output = counter(usage, &["completion_tokens"])
        .or_else(|| counter(usage, &["output_tokens"]))
        .or_else(|| counter(usage, &["total_output_tokens"]))
        .or_else(|| counter(usage, &["eval_count"]));
    let cached = counter(usage, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| counter(usage, &["input_tokens_details", "cached_tokens"]))
        .or_else(|| counter(usage, &["cached_tokens"]))
        .filter(|n| *n <= prompt);
    let model = identifier(record, "model")
        .or_else(|| identifier(record, "model_instance_id"))
        .unwrap_or_else(|| "unknown".into());
    Some(LlmTelemetry {
        source: TelemetrySource::Report,
        observed_at: Some(observed_at),
        provider: Some(provider.into()),
        model: Some(model.clone()),
        status: Some("last result".into()),
        prompt_tokens: Some(prompt),
        output_tokens: output,
        requests: vec![RequestUsage {
            provider: provider.into(),
            model,
            id,
            prompt,
            output,
            cached,
            completed: true,
            ttft_ms: counter(record, &["timings", "time_to_first_token_ms"]),
            observed_at: Some(observed_at),
        }],
        ..LlmTelemetry::default()
    })
}

fn read_usage_file(path: &Path, provider: Option<&str>) -> Option<LlmTelemetry> {
    // Avoid blocking on a FIFO accidentally configured as a usage file.
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let start = metadata.len().saturating_sub(MAX_USAGE_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_USAGE_BYTES).read_to_end(&mut bytes).ok()?;
    // Only newline-terminated records are complete. Skip any partial first line.
    let end = bytes.iter().rposition(|b| *b == b'\n')?;
    let start = if start > 0 {
        bytes.iter().position(|b| *b == b'\n')? + 1
    } else {
        0
    };
    if start > end {
        return None;
    }
    let mut latest: Option<LlmTelemetry> = None;
    let mut requests = Vec::new();
    for line in bytes[start..end].split(|b| *b == b'\n') {
        let Some(record) = serde_json::from_slice::<Value>(line)
            .ok()
            .and_then(|v| parse_usage(&v))
        else {
            continue;
        };
        if provider.is_some_and(|selected| record.provider.as_deref() != Some(selected)) {
            continue;
        }
        requests.extend(record.requests.clone());
        if latest
            .as_ref()
            .is_none_or(|old| record.observed_at >= old.observed_at)
        {
            latest = Some(record);
        }
    }
    let mut latest = latest?;
    let selected = latest.provider.as_deref();
    let mut seen = std::collections::HashSet::new();
    latest.requests = requests
        .into_iter()
        .rev()
        .filter(|request| Some(request.provider.as_str()) == selected)
        .filter(|request| {
            seen.insert((
                request.provider.clone(),
                request.model.clone(),
                request.id.clone(),
            ))
        })
        .take(128)
        .collect();
    latest.requests.reverse();
    Some(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(provider: &str, usage: Value) -> Value {
        json!({"provider": provider, "request_id": "req-1", "model": "test-model",
            "observed_at": 1700000000_u64, "usage": usage})
    }

    #[test]
    fn llama_sums_active_slots_without_inventing_missing_output() {
        let mut slots = json!([
            {"is_processing":true,"next_token":{"n_decoded":12}},
            {"is_processing":true,"next_token":{"n_decoded":8}},
            {"is_processing":false,"next_token":{"n_decoded":999}}
        ]);
        let result = parse_llama_slots(&slots).unwrap();
        assert_eq!(result.active_requests, Some(2));
        assert_eq!(result.output_tokens, Some(20));
        let sample = Sample {
            llm_output_tokens: result.output_tokens,
            ..Sample::default()
        };
        assert_eq!(llm_context_tokens(&sample), None);
        slots[1]["next_token"] = json!({});
        assert_eq!(parse_llama_slots(&slots).unwrap().output_tokens, None);
    }

    #[test]
    fn llama_metrics_work_without_slots_and_keep_average_rates() {
        let metrics = "# TYPE llamacpp:requests_processing gauge\nllamacpp:requests_processing 2\nllamacpp:requests_deferred 3\nllamacpp:predicted_tokens_seconds 24.5\nllamacpp:prompt_tokens_seconds 100\n";
        let result = merge_llama_metrics(None, Some(metrics)).unwrap();
        assert_eq!(result.active_requests, Some(2));
        assert_eq!(result.waiting_requests, Some(3));
        assert_eq!(result.generation_tps, Some(24.5));
        assert_eq!(result.prefill_tps, Some(100.0));
        assert!(!result.generation_tps_live);
        assert!(!result.prefill_tps_live);
        assert_eq!(result.prompt_tokens, None);
        assert!(merge_llama_metrics(None, Some("<html>disabled</html>")).is_none());
        assert_eq!(metric_count("count 1.5", "count"), None);
        assert_eq!(metric_count("count -1", "count"), None);
        assert_eq!(metric_count("count +Inf", "count"), None);
        let idle = parse_llama_slots(&json!([]));
        assert_eq!(
            merge_llama_metrics(idle, None).unwrap().active_requests,
            Some(0)
        );
    }

    #[test]
    fn response_usage_and_incomplete_ollama_chunks() {
        let usage = parse_usage(&record(
            "LocalAI",
            json!({"input_tokens":50,
            "output_tokens":4,"input_tokens_details":{"cached_tokens":20}}),
        ))
        .unwrap();
        assert_eq!(usage.output_tokens, Some(4));
        assert_eq!(usage.requests[0].cached, Some(20));
        let mut streaming = record("Ollama", json!({"prompt_eval_count":50}));
        streaming["done"] = json!(false);
        assert!(parse_usage(&streaming).is_none());
    }

    #[test]
    fn usage_file_supplements_live_slots_and_filters_other_providers() {
        let path = env::temp_dir().join(format!("mlxtop-mixed-{}.jsonl", std::process::id()));
        let llama = record(
            "llama-server",
            json!({"prompt_tokens":400,"completion_tokens":30}),
        );
        let mut ollama = record("ollama", json!({"prompt_eval_count":999}));
        ollama["observed_at"] = json!(1700000001);
        fs::write(&path, format!("{llama}\n{llama}\n{ollama}\n")).unwrap();
        let adapter = Adapter {
            configured: Some("llama.cpp".into()),
            usage_file: Some(path.clone()),
            selected: Some("llama.cpp".into()),
            port: None,
            cached: parse_llama_slots(
                &json!([{"is_processing":true,"next_token":{"n_decoded":5}}]),
            ),
            next_poll: Instant::now(),
            backoff: Duration::from_secs(1),
            kobold_uptime: None,
            kobold_session: 0,
        };
        let result = adapter.with_usage().unwrap();
        assert_eq!(result.source, TelemetrySource::Live);
        assert_eq!(result.active_requests, Some(1));
        assert_eq!(result.output_tokens, Some(5));
        assert_eq!(result.prompt_tokens, None);
        assert_eq!(result.requests.len(), 1);
        assert_eq!(result.requests[0].prompt, 400);
        assert!(result.requests[0].completed);
        assert_eq!(read_usage_file(&path, None).unwrap().requests.len(), 1);
        fs::write(&path, "invalid\n").unwrap();
        assert_eq!(adapter.with_usage().unwrap().active_requests, Some(1));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn llama_http_polls_metrics_even_when_slots_are_disabled() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for path in ["/slots", "/metrics"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = [0; 2048];
                let n = stream.read(&mut bytes).unwrap();
                assert!(String::from_utf8_lossy(&bytes[..n])
                    .starts_with(&format!("GET {path} HTTP/1.1")));
                let (status, body) = if path == "/slots" {
                    (403, "{}")
                } else {
                    (
                        200,
                        "llamacpp:requests_processing 1\nllamacpp:requests_deferred 2\n",
                    )
                };
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut adapter = Adapter {
            configured: None,
            usage_file: None,
            selected: Some("llama.cpp".into()),
            port: Some(port),
            cached: None,
            next_poll: Instant::now(),
            backoff: Duration::from_secs(1),
            kobold_uptime: None,
            kobold_session: 0,
        };
        let result = adapter.poll().unwrap();
        assert_eq!(result.waiting_requests, Some(2));
        server.join().unwrap();
    }

    #[test]
    fn first_token_timing_is_explicit_not_derived_from_provider_durations() {
        let mut value = record(
            "Ollama",
            json!({"prompt_tokens": 100, "prompt_eval_duration": 999999}),
        );
        assert_eq!(parse_usage(&value).unwrap().requests[0].ttft_ms, None);
        value["timings"] = json!({"time_to_first_token_ms": 1250});
        assert_eq!(parse_usage(&value).unwrap().requests[0].ttft_ms, Some(1250));
        value["timings"] = json!({"time_to_first_token_ms": -1});
        assert_eq!(parse_usage(&value).unwrap().requests[0].ttft_ms, None);
    }

    #[test]
    fn response_formats_preserve_full_prompt_and_cache_counts() {
        for provider in ["omlx", "mlx_lm.server", "llama.cpp", "koboldcpp", "localai"] {
            let telemetry = parse_usage(&record(
                provider,
                json!({
                    "prompt_tokens": 10000, "completion_tokens": 12,
                    "prompt_tokens_details": {"cached_tokens": 9000}
                }),
            ))
            .unwrap();
            assert_eq!(telemetry.prompt_tokens, Some(10000));
            assert_eq!(telemetry.requests[0].cached, Some(9000));
            assert_eq!(telemetry.source, TelemetrySource::Report);
            assert!(!telemetry.generation_tps_live);
            assert!(telemetry.cache_efficiency.is_none()); // no aggregate/request mixing
        }
        let ollama = parse_usage(&record(
            "Ollama",
            json!({"prompt_eval_count": 800, "eval_count": 42}),
        ))
        .unwrap();
        assert_eq!(ollama.prompt_tokens, Some(800));
        assert_eq!(ollama.output_tokens, Some(42));
        let lmstudio = parse_usage(
            &json!({"provider":"LM Studio", "request_id":"r", "observed_at":1700000000,
            "stats":{"input_tokens":333,"total_output_tokens":22}}),
        )
        .unwrap();
        assert_eq!(lmstudio.prompt_tokens, Some(333));
    }

    #[test]
    fn missing_invalid_and_future_usage_stays_unavailable() {
        for usage in [
            json!({}),
            json!({"prompt_tokens": -1}),
            json!({"prompt_tokens": 1.5}),
            json!({"prompt_tokens":"123"}),
        ] {
            assert!(parse_usage(&record("Ollama", usage)).is_none());
        }
        let mut value = record("mlx-lm", json!({"prompt_tokens":0,"cached_tokens":1}));
        let parsed = parse_usage(&value).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(0));
        assert_eq!(parsed.requests[0].cached, None);
        value["observed_at"] = json!(u64::MAX);
        assert!(parse_usage(&value).is_none());
        value["observed_at"] = json!(1700000000);
        value.as_object_mut().unwrap().remove("request_id");
        assert!(parse_usage(&value).is_none());
    }

    #[test]
    fn omlx_collects_every_model_and_request_without_aggregate_cache() {
        let requests = omlx_requests(&json!({"cache_efficiency":99,"active_models":{"models":[
            {"id":"a","generating":[{"request_id":"1","prompt_tokens":100},{"request_id":"2","prompt_tokens":200}],
             "waiting":[{"request_id":"3","prompt_tokens":0}]},
            {"id":"b","prefilling":[{"request_id":"4","prompt_tokens":300,"cached_tokens":100}]}
        ]}}));
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].prompt, 200);
        assert_eq!(requests[0].cached, None);
        assert_eq!(requests[2].cached, Some(100));
    }

    #[test]
    fn journal_deduplicates_polling_but_preserves_equal_sized_requests() {
        let mut seen = VecDeque::new();
        let mut request = parse_usage(&record("Ollama", json!({"prompt_tokens":10})))
            .unwrap()
            .requests
            .remove(0);
        assert!(new_request_summary(&mut seen, &request).is_some());
        assert!(new_request_summary(&mut seen, &request).is_none());
        request.id = "req-2".into();
        assert!(new_request_summary(&mut seen, &request).is_some());
        for n in 0..600 {
            request.id = n.to_string();
            new_request_summary(&mut seen, &request);
        }
        assert_eq!(seen.len(), 512);
    }

    #[test]
    fn kobold_last_result_never_becomes_a_live_rate() {
        let result = parse_kobold(
            &json!({"total_gens":2,"last_input_count":1000,"last_token_count":20,
            "last_eval_speed":30,"last_process_speed":500,"idle":0}),
        )
        .unwrap();
        assert_eq!(result.prompt_tokens, Some(1000));
        assert_eq!(result.source, TelemetrySource::Report);
        assert_eq!(result.status.as_deref(), Some("last result"));
        assert!(!result.generation_tps_live);
        assert_eq!(
            parse_kobold(&json!({"total_gens":0,"last_input_count":0}))
                .unwrap()
                .prompt_tokens,
            None
        );
        assert!(parse_kobold(&json!({"status":"ok"})).is_none());
    }

    #[test]
    fn kobold_restart_does_not_reuse_request_identity() {
        let mut adapter = Adapter {
            configured: None,
            usage_file: None,
            selected: Some("KoboldCpp".into()),
            port: None,
            cached: None,
            next_poll: Instant::now(),
            backoff: Duration::from_secs(1),
            kobold_uptime: None,
            kobold_session: 0,
        };
        let first = adapter
            .kobold_result(&json!({"total_gens":1,"last_input_count":100,"uptime":500.5}))
            .unwrap();
        let repeated = adapter
            .kobold_result(&json!({"total_gens":1,"last_input_count":100,"uptime":501.5}))
            .unwrap();
        let restarted = adapter
            .kobold_result(&json!({"total_gens":1,"last_input_count":200,"uptime":5.0}))
            .unwrap();
        assert_eq!(first.requests[0].id, repeated.requests[0].id);
        assert_ne!(first.requests[0].id, restarted.requests[0].id);
        assert_eq!(restarted.source, TelemetrySource::Report);
    }

    #[test]
    fn llama_capacity_and_processed_work_are_not_prompt_length() {
        let slots = json!([{"id":0,"id_task":10,"is_processing":true,"n_ctx":65536,
            "timings":{"prompt_n":50},"next_token":{"n_decoded":12}},
            {"id":1,"is_processing":false,"next_token":{"n_decoded":999}}]);
        let result = parse_llama_slots(&slots).unwrap();
        assert_eq!(result.active_requests, Some(1));
        assert_eq!(result.output_tokens, Some(12));
        assert_eq!(result.prompt_tokens, None);
        assert!(parse_llama_slots(&json!([{}])).is_none());
        let idle = parse_llama_slots(&json!([])).unwrap();
        assert_eq!(idle.active_requests, Some(0));
        assert_eq!(idle.output_tokens, None);
        assert_eq!(
            metric(
                "llamacpp:predicted_tokens_seconds 25\n",
                "llamacpp:predicted_tokens_seconds"
            ),
            Some(25.0)
        );
        assert_eq!(metric("rate NaN\n", "rate"), None);
    }

    #[test]
    fn usage_file_ignores_partial_and_bad_records_and_keeps_original_time() {
        let path = env::temp_dir().join(format!(
            "mlxtop-usage-test-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let valid = record("mlx-lm", json!({"prompt_tokens":1024}));
        fs::write(&path, format!("not json\n{valid}\n{{\"partial\":")).unwrap();
        let result = read_usage_file(&path, None).unwrap();
        assert_eq!(result.prompt_tokens, Some(1024));
        assert_eq!(result.requests.len(), 1);
        assert_eq!(
            result.observed_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000))
        );
        fs::write(&path, valid.to_string()).unwrap();
        assert!(read_usage_file(&path, None).is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn provider_switch_drops_cached_counts() {
        let mut adapter = Adapter {
            configured: None,
            usage_file: None,
            selected: Some("KoboldCpp".into()),
            port: None,
            cached: parse_kobold(&json!({"total_gens":1,"last_input_count":20})),
            next_poll: Instant::now(),
            backoff: Duration::from_secs(30),
            kobold_uptime: None,
            kobold_session: 0,
        };
        assert!(adapter.selected(Some("Ollama")));
        assert!(adapter.cached.is_none());
        assert!(adapter.poll().is_none());
        assert!(!adapter.selected(Some("oMLX")));
    }

    #[test]
    fn native_poll_uses_get_and_preserves_last_result_age() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = [0; 2048];
                let n = stream.read(&mut bytes).unwrap();
                assert!(String::from_utf8_lossy(&bytes[..n])
                    .starts_with("GET /api/extra/perf HTTP/1.1"));
                let body = r#"{"total_gens":1,"last_input_count":456}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut adapter = Adapter {
            configured: Some("KoboldCpp".into()),
            usage_file: None,
            selected: Some("KoboldCpp".into()),
            port: Some(port),
            cached: None,
            next_poll: Instant::now(),
            backoff: Duration::from_secs(1),
            kobold_uptime: None,
            kobold_session: 0,
        };
        let first = adapter.poll().unwrap();
        adapter.next_poll = Instant::now();
        let second = adapter.poll().unwrap();
        assert_eq!(first.prompt_tokens, Some(456));
        assert_eq!(first.observed_at, second.observed_at);
        server.join().unwrap();
    }
}
