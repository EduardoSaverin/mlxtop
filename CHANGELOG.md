# Changelog

## 1.1.0 — 2026-09-15

### Operator dashboard

- Integrate prompt load into the wide Overview grid beside generation and
  prefill, retaining a stacked layout for smaller terminals.
- Show request input counts, freshness, previous-request changes and recent
  comparisons. Display cached/uncached segments only when reported for that
  request, with bounded history and keyboard navigation.
- Add active/waiting queue history and OS process-footprint charts. Keep
  missing readings as gaps and identify the measured process by PID.
- Show macOS process footprint, lifetime peak and signed memory growth without
  modifying or restarting the serving runtime.
- Display first-token latency only when explicit client timing is supplied.
  Otherwise, retain the recent Journal preview in that space.
- Document consistent title casing, colors, layout and telemetry semantics.

### Providers and platforms

- Add request telemetry adapters for oMLX, llama.cpp and KoboldCpp, plus an
  optional counters-only usage file for client-reported responses, including
  MLX-LM, Ollama, LM Studio and LocalAI.
- Distinguish live observations from retained or reported results. Polling may
  miss requests that finish between samples; timing and cache data are never
  inferred from unrelated counters.
- Include Linux support using `/proc`, `/sys` and optional `nvidia-smi`.
  The published DMG remains for Apple Silicon macOS; Linux uses source builds.
- Add the macOS-only `libproc` dependency and its dependency license notices.

### Distribution

- Publish an Apple Silicon DMG with a native installer, documentation,
  dependency notices and SHA-256 checksum.
- Update the terminal installer and download links to v1.1.0.
- The binary targets macOS 11 or later and is tested on macOS 26.5.1. The
  installer is unsigned and the release is not Apple notarized.

## 1.0.0 — 2026-09-11

- Initial release with Overview, MLX Top, Journal, oMLX serving telemetry,
  macOS memory/paging/GPU inspection and the `--once` static report.
- Apple Silicon distribution with checksum verification and license notices.
