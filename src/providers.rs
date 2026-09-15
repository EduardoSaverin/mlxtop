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
        }
        self.usage_file.is_some() || self.selected.as_deref().is_some_and(|s| s != "oMLX")
    }

    pub fn poll(&mut self) -> Option<LlmTelemetry> {
        let now = Instant::now();
        if now < self.next_poll {
            return self.cached.clone();
        }
        let result = if let Some(path) = &self.usage_file {
            read_usage_file(path)
        } else {
            match self.selected.as_deref() {
                Some("KoboldCpp") => self.poll_kobold(),
                Some("llama.cpp") => self.poll_llama(),
                _ => None,
            }
        };
        if let Some(mut result) = result {
            // A successful poll does not make KoboldCpp's last completion new.
            if self.usage_file.is_none() && result.provider.as_deref() == Some("KoboldCpp") {
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
        self.cached.clone()
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

    fn poll_kobold(&self) -> Option<LlmTelemetry> {
        let perf: Value = serde_json::from_str(&self.get(5001, "/api/extra/perf")?).ok()?;
        parse_kobold(&perf)
    }

    fn poll_llama(&self) -> Option<LlmTelemetry> {
        let slots: Value = serde_json::from_str(&self.get(8080, "/slots")?).ok()?;
        let mut telemetry = parse_llama_slots(&slots)?;
        if let Some(metrics) = self.get(8080, "/metrics") {
            telemetry.generation_tps = metric(&metrics, "llamacpp:predicted_tokens_seconds");
            telemetry.prefill_tps = metric(&metrics, "llamacpp:prompt_tokens_seconds");
            // These are aggregate averages, never live request rates.
        }
        Some(telemetry)
    }
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
        output_tokens: active
            .first()
            .and_then(|s| counter(s, &["next_token", "n_decoded"])),
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
        .or_else(|| counter(usage, &["total_output_tokens"]))
        .or_else(|| counter(usage, &["eval_count"]));
    let cached = counter(usage, &["prompt_tokens_details", "cached_tokens"])
        .or_else(|| counter(usage, &["cached_tokens"]))
        .filter(|n| *n <= prompt);
    let model = identifier(record, "model").unwrap_or_else(|| "unknown".into());
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

fn read_usage_file(path: &Path) -> Option<LlmTelemetry> {
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
        requests.extend(record.requests.clone());
        if latest
            .as_ref()
            .is_none_or(|old| record.observed_at >= old.observed_at)
        {
            latest = Some(record);
        }
    }
    let mut latest = latest?;
    latest.requests = requests.into_iter().rev().take(128).collect();
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
        let result = read_usage_file(&path).unwrap();
        assert_eq!(result.prompt_tokens, Some(1024));
        assert_eq!(result.requests.len(), 1);
        assert_eq!(
            result.observed_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000))
        );
        fs::write(&path, valid.to_string()).unwrap();
        assert!(read_usage_file(&path).is_none());
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
        };
        let first = adapter.poll().unwrap();
        adapter.next_poll = Instant::now();
        let second = adapter.poll().unwrap();
        assert_eq!(first.prompt_tokens, Some(456));
        assert_eq!(first.observed_at, second.observed_at);
        server.join().unwrap();
    }
}
