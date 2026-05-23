from __future__ import annotations

from dataclasses import dataclass
from math import ceil, isfinite
from statistics import median
import os
import platform
import shutil
import sqlite3
import subprocess
import sys

from hymt import __version__
from hymt.history import DurationEstimate, HistoryDB, TaskRecord, format_duration


__all__ = [
    "TimingIssueData",
    "maybe_prompt_timing_issue",
]

ISSUE_REPO = "RyderFreeman4Logos/hymt"
RECENT_HISTORY_LIMIT = 10


@dataclass(frozen=True)
class TimingIssueData:
    input_tokens: int
    output_tokens: int
    segments: int
    actual_seconds: float
    estimated_seconds: float
    config_version: int
    target_lang: str
    template_type: str
    concurrency: int
    model: str | None

    @property
    def ratio(self) -> float:
        if self.estimated_seconds <= 0:
            return 0.0
        return self.actual_seconds / self.estimated_seconds


@dataclass(frozen=True)
class _RecentStats:
    count: int
    avg_tokens_per_second: float
    median_tokens_per_second: float
    p5_tokens_per_second: float
    p95_tokens_per_second: float


def maybe_prompt_timing_issue(
    history: HistoryDB,
    estimate: DurationEstimate | None,
    data: TimingIssueData,
    threshold: float,
) -> None:
    if estimate is None or estimate.seconds <= 0 or not _is_divergent(data, threshold):
        return
    if not sys.stdin.isatty():
        return

    prompt = (
        f"Actual time ({format_duration(data.actual_seconds)}) differs significantly "
        f"from estimate ({format_duration(data.estimated_seconds)}). "
        "File an issue with timing data? (y/n) "
    )
    print(prompt, end="", file=sys.stderr, flush=True)
    response = sys.stdin.readline().strip().lower()
    if response != "y":
        return

    body = build_timing_issue_body(history, data)
    create_timing_issue(data, body)


def build_timing_issue_body(history: HistoryDB, data: TimingIssueData) -> str:
    records = _fetch_recent(history)
    stats = _stats_from_records(records)
    return "\n".join(
        [
            "## Timing divergence",
            "",
            "| Field | Value |",
            "| --- | --- |",
            f"| Actual duration | {format_duration(data.actual_seconds)} |",
            f"| Estimated duration | {format_duration(data.estimated_seconds)} |",
            f"| Ratio | {data.ratio:.2f}x |",
            f"| Input tokens | {data.input_tokens} |",
            f"| Output tokens | {data.output_tokens} |",
            f"| Segments | {data.segments} |",
            f"| Concurrency | {data.concurrency} |",
            f"| Target language | {data.target_lang} |",
            f"| Template type | {data.template_type} |",
            f"| Model | {data.model or 'not configured'} |",
            "",
            f"## Historical token/s statistics (last {RECENT_HISTORY_LIMIT} tasks)",
            "",
            _format_recent_stats(stats),
            "",
            _format_recent_records(records),
            "",
            "## Environment",
            "",
            _format_environment(data.config_version),
        ]
    )


def create_timing_issue(data: TimingIssueData, body: str) -> None:
    if shutil.which("gh") is None:
        print("Warning: gh CLI not found; timing issue not filed.", file=sys.stderr)
        return

    title = (
        "Timing estimate divergence: "
        f"actual {format_duration(data.actual_seconds)} vs "
        f"estimate {format_duration(data.estimated_seconds)}"
    )
    result = subprocess.run(
        [
            "gh",
            "issue",
            "create",
            "--repo",
            ISSUE_REPO,
            "--title",
            title,
            "--body",
            body,
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode == 0:
        issue_url = result.stdout.strip()
        if issue_url:
            print(f"Filed timing issue: {issue_url}", file=sys.stderr)
        return

    detail = (result.stderr or result.stdout).strip()
    suffix = f": {detail}" if detail else f" (exit {result.returncode})"
    print(f"Warning: failed to file timing issue with gh{suffix}", file=sys.stderr)


def _is_divergent(data: TimingIssueData, threshold: float) -> bool:
    effective_threshold = threshold if threshold > 1.0 else 2.0
    ratio = data.ratio
    return ratio > effective_threshold or ratio < 1.0 / effective_threshold


def _fetch_recent(history: HistoryDB) -> list[TaskRecord]:
    try:
        return history.fetch_recent(RECENT_HISTORY_LIMIT)
    except (OSError, sqlite3.Error) as exc:
        print(
            f"Warning: failed to read timing history for issue body: {exc}",
            file=sys.stderr,
        )
        return []


def _stats_from_records(records: list[TaskRecord]) -> _RecentStats | None:
    rates = sorted(
        record.tokens_per_second for record in records if record.tokens_per_second > 0
    )
    if not rates:
        return None
    return _RecentStats(
        count=len(rates),
        avg_tokens_per_second=sum(rates) / len(rates),
        median_tokens_per_second=float(median(rates)),
        p5_tokens_per_second=_percentile(rates, 0.05),
        p95_tokens_per_second=_percentile(rates, 0.95),
    )


def _percentile(sorted_values: list[float], percentile: float) -> float:
    if not sorted_values:
        return 0.0
    index = min(
        len(sorted_values) - 1, max(0, ceil(percentile * len(sorted_values)) - 1)
    )
    return sorted_values[index]


def _format_recent_stats(stats: _RecentStats | None) -> str:
    if stats is None:
        return "No historical token/s data available."
    return "\n".join(
        [
            f"- Tasks: {stats.count}",
            f"- Average: {stats.avg_tokens_per_second:.1f} tok/s",
            f"- Median: {stats.median_tokens_per_second:.1f} tok/s",
            f"- p5: {stats.p5_tokens_per_second:.1f} tok/s",
            f"- p95: {stats.p95_tokens_per_second:.1f} tok/s",
        ]
    )


def _format_recent_records(records: list[TaskRecord]) -> str:
    if not records:
        return "No recent tasks available."
    rows = [
        "| Finished | Tokens/s | Duration | Input tokens | Output tokens | Segments |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for record in records:
        rows.append(
            "| "
            f"{record.finished_at} | "
            f"{record.tokens_per_second:.1f} | "
            f"{format_duration(record.duration_seconds)} | "
            f"{record.input_tokens} | "
            f"{record.output_tokens} | "
            f"{record.segments} |"
        )
    return "\n".join(rows)


def _format_environment(config_version: int) -> str:
    hardware = _detect_hardware()
    return "\n".join(
        [
            f"- hymt version: {__version__}",
            f"- config_version: {config_version}",
            f"- OS/platform: {platform.platform()}",
            f"- Machine: {platform.machine()}",
            f"- CPU: {hardware.cpu}",
            f"- RAM: {hardware.ram}",
            f"- GPU: {hardware.gpu}",
        ]
    )


@dataclass(frozen=True)
class _HardwareInfo:
    cpu: str
    ram: str
    gpu: str


def _detect_hardware() -> _HardwareInfo:
    cpu_name = platform.processor() or platform.machine() or "unknown"
    cpu_count = os.cpu_count()
    cpu = f"{cpu_name} ({cpu_count} logical cores)" if cpu_count else cpu_name
    return _HardwareInfo(cpu=cpu, ram=_detect_ram(), gpu=_detect_gpu())


def _detect_ram() -> str:
    meminfo = _linux_mem_total()
    if meminfo is not None:
        return _format_bytes(meminfo)
    if hasattr(os, "sysconf"):
        try:
            page_size = int(os.sysconf("SC_PAGE_SIZE"))
            page_count = int(os.sysconf("SC_PHYS_PAGES"))
        except (OSError, ValueError):
            return "unknown"
        total = page_size * page_count
        return _format_bytes(total) if total > 0 else "unknown"
    return "unknown"


def _linux_mem_total() -> int | None:
    try:
        with open("/proc/meminfo", encoding="utf-8") as meminfo:
            for line in meminfo:
                if not line.startswith("MemTotal:"):
                    continue
                parts = line.split()
                if len(parts) < 2:
                    return None
                return int(parts[1]) * 1024
    except (OSError, ValueError):
        return None
    return None


def _format_bytes(value: int) -> str:
    if value <= 0 or not isfinite(value):
        return "unknown"
    gib = value / 1024**3
    return f"{gib:.1f} GiB"


def _detect_gpu() -> str:
    if shutil.which("nvidia-smi") is None:
        return "not detected"
    try:
        result = subprocess.run(
            [
                "nvidia-smi",
                "--query-gpu=name,memory.total",
                "--format=csv,noheader",
            ],
            check=False,
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return "not detected"
    if result.returncode != 0:
        return "not detected"
    gpu = "; ".join(line.strip() for line in result.stdout.splitlines() if line.strip())
    return gpu or "not detected"
