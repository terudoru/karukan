#!/usr/bin/env python3
"""Run a timed semantic/live-conversion improvement loop against karukan-imserver.

The interaction probes mirror state invariants used by other open-source IMEs:

- Mozc keeps composition, suggestion/prediction, and conversion as distinct
  states instead of treating a prediction as committed input.
- libskk tests explicit conversion as a sequence of key operations and supports
  re-conversion.
- Rime keeps raw input/caret state separate from the rendered composition and
  can reopen a selected segment for editing.

Primary references are recorded in each JSON report so a regression can be
traced back to the behavior that motivated its probe.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import datetime as dt
import hashlib
import json
import os
import selectors
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


DEFAULT_SERVER = Path.home() / (
    "Library/Input Methods/Karukan.app/Contents/MacOS/karukan-imserver"
)

OSS_BEHAVIOR_REFERENCES = (
    {
        "project": "Mozc",
        "url": (
            "https://chromium.googlesource.com/external/mozc/src/+/master/"
            "session/session_converter.cc"
        ),
        "behavior": "separate composition, suggestion/prediction, and conversion states",
    },
    {
        "project": "libskk",
        "url": "https://github.com/ueno/libskk",
        "behavior": "key-sequence interaction tests and re-conversion",
    },
    {
        "project": "librime",
        "url": "https://github.com/rime/librime/blob/master/src/rime/context.cc",
        "behavior": "raw input and caret remain separate from editable composition",
    },
)

KEY_RETURN = 0xFF0D
KEY_ESCAPE = 0xFF1B
KEY_LEFT = 0xFF51
KEY_SPACE = 0x0020


@dataclasses.dataclass(frozen=True)
class Case:
    name: str
    category: str
    segments: tuple[str, ...]
    expected: tuple[str, ...] = ()


CASES = (
    Case(
        "dentist",
        "common",
        ("kyou", "haisha", "ni", "iki", "masu"),
        ("今日歯医者に行きます", "今日は医者に行きます"),
    ),
    Case(
        "tomorrow_meeting",
        "common",
        ("ashita", "toukyou", "de", "kaigi", "ga", "ari", "masu"),
        ("明日東京で会議があります",),
    ),
    Case(
        "weather",
        "common",
        ("kesa", "ha", "harete", "ima", "suga", "gogo", "kara", "ame", "desu"),
    ),
    Case(
        "investigation",
        "common",
        ("kono", "mondai", "no", "genninn", "wo", "shirabe", "masu"),
        ("この問題の原因を調べます",),
    ),
    Case(
        "university_report",
        "common",
        ("daigaku", "no", "jikkenn", "kekka", "wo", "houkoku", "shi", "masu"),
    ),
    Case(
        "software_update",
        "technical",
        ("atarashii", "sofutowea", "wo", "innsutooru", "shi", "mashita"),
        ("新しいソフトウェアをインストールしました",),
    ),
    Case(
        "server_restart",
        "technical",
        ("sa-ba-", "wo", "saikidou", "shite", "rogu", "wo", "kakuninn", "shi", "masu"),
        ("サーバーを再起動してログを確認します",),
    ),
    Case(
        "network_latency",
        "technical",
        ("nettowa-ku", "no", "chienn", "wo", "sokutei", "shi", "masu"),
        ("ネットワークの遅延を測定します",),
    ),
    Case(
        "bridge_crossing",
        "homophone",
        ("hashi", "wo", "watatte", "eki", "he", "iki", "masu"),
        ("橋を渡って駅へ行きます",),
    ),
    Case(
        "chopsticks",
        "homophone",
        ("hashi", "de", "gohann", "wo", "tabe", "masu"),
        ("箸でご飯を食べます",),
    ),
    Case(
        "rain_falls",
        "homophone",
        ("ame", "ga", "futte", "ki", "mashita"),
        ("雨が降ってきました",),
    ),
    Case(
        "candy",
        "homophone",
        ("amai", "ame", "wo", "name", "masu"),
        ("甘い飴をなめます",),
    ),
    Case(
        "paper",
        "homophone",
        ("shiroi", "kami", "ni", "moji", "wo", "kaki", "masu"),
        ("白い紙に文字を書きます",),
    ),
    Case(
        "hair",
        "homophone",
        ("kami", "wo", "mijikaku", "kiri", "mashita"),
        ("髪を短く切りました",),
    ),
    Case(
        "year_date",
        "digits",
        ("2026", "nenn", "7", "gatsu", "23", "nichi", "desu"),
        ("2026年7月23日です",),
    ),
    Case(
        "ip_address",
        "digits",
        ("aipi-", "adoresu", "ha", "10.1.30.3", "desu"),
        ("IPアドレスは10。1。30。3です", "アイピーアドレスは10。1。30。3です"),
    ),
    Case(
        "version",
        "digits",
        ("ba-jonn", "2.0.1", "wo", "riri-su", "shi", "masu"),
        ("バージョン2。0。1をリリースします",),
    ),
    Case(
        "quantity",
        "digits",
        ("123456", "kenn", "no", "de-ta", "wo", "kakuninn", "shi", "mashita"),
        ("123456件のデータを確認しました",),
    ),
    Case(
        "parentheses",
        "symbols",
        ("kakko", "(", "tesuto", ")", "wo", "nyuuryoku", "shi", "masu"),
    ),
    Case(
        "punctuation",
        "symbols",
        ("kyou", "ha", ",", "ame", "desu", ".", "ashita", "ha", "hare", "desu", "."),
    ),
    Case(
        "quoted_word",
        "symbols",
        ("karukann", "ha", "\"", "nihonngo", "\"", "no", "nyuuryoku", "hou", "desu"),
    ),
    Case(
        "question",
        "symbols",
        ("kono", "kekka", "ha", "tadashii", "desu", "ka", "?"),
    ),
    Case(
        "polite_request",
        "grammar",
        ("kakuninn", "shite", "itadake", "masu", "ka"),
        ("確認していただけますか",),
    ),
    Case(
        "negative",
        "grammar",
        ("mada", "kekka", "ha", "dete", "i", "mase", "nn"),
        ("まだ結果は出ていません",),
    ),
    Case(
        "conditional",
        "grammar",
        ("ame", "nara", "densha", "de", "iki", "masu"),
        ("雨なら電車で行きます",),
    ),
    Case(
        "long_no_punctuation",
        "long",
        (
            "watashi", "ha", "kyou", "daigaku", "no", "kenkyuushitsu", "de",
            "jikkenn", "no", "kekka", "wo", "seiri", "shite", "sensei", "ni",
            "houkoku", "suru", "tame", "no", "shiryou", "wo", "sakusei", "shi",
            "mashita",
        ),
        ("私は今日大学の研究室で実験の結果を整理して先生に報告するための資料を作成しました",),
    ),
    Case(
        "long_with_punctuation",
        "long",
        (
            "kesa", "ha", "hayaku", "oki", "mashita", ".", "sorekara", "asa",
            "gohann", "wo", "tabe", ",", "densha", "de", "daigaku", "he", "iki",
            "mashita", ".", "gogo", "ha", "tomodachi", "to", "kaeri", "masu", ".",
        ),
    ),
    Case(
        "repeated_words",
        "boundary",
        ("kore", "kara", "kara", "no", "yotei", "wo", "kaku", "ninn", "shi", "masu"),
    ),
    Case(
        "small_kana",
        "boundary",
        ("shucchou", "chuuni", "shashin", "wo", "satsuei", "shi", "mashita"),
    ),
    Case(
        "sokuon",
        "boundary",
        ("kitto", "motto", "yoku", "nari", "masu"),
        ("きっともっと良くなります",),
    ),
    Case(
        "katakana_loanwords",
        "loanword",
        ("konpyu-ta-", "to", "ki-bo-do", "wo", "tsukai", "masu"),
        ("コンピューターとキーボードを使います",),
    ),
    Case(
        "mixed_loanwords",
        "loanword",
        ("purojekuto", "no", "sukeju-ru", "wo", "appude-to", "shi", "masu"),
        ("プロジェクトのスケジュールをアップデートします",),
    ),
)


class RpcClient:
    def __init__(self, server: Path, timeout: float = 15.0) -> None:
        self.server = server
        self.timeout = timeout
        self.next_id = 1
        self.proc = subprocess.Popen(
            [str(server)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        assert self.proc.stdout is not None
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)

    def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        if self.proc.poll() is not None:
            raise RuntimeError(f"karukan-imserver exited with {self.proc.returncode}")
        request_id = self.next_id
        self.next_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        }
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()
        events = self.selector.select(self.timeout)
        if not events:
            raise TimeoutError(f"{method} timed out after {self.timeout:.1f}s")
        assert self.proc.stdout is not None
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(f"karukan-imserver closed stdout during {method}")
        response = json.loads(line)
        if response.get("id") != request_id:
            raise RuntimeError(f"unexpected response id: {response!r}")
        if "error" in response:
            raise RuntimeError(f"{method} failed: {response['error']}")
        return response["result"]

    def close(self) -> None:
        if self.proc.stdin is not None:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            self.proc.wait(timeout=5)


def last_preedit(result: dict[str, Any]) -> str | None:
    values = [
        action["text"]
        for action in result.get("actions", [])
        if action.get("type") == "update_preedit"
    ]
    return values[-1] if values else None


def last_preedit_state(result: dict[str, Any]) -> tuple[str, int] | None:
    values = [
        (action["text"], action["caret"])
        for action in result.get("actions", [])
        if action.get("type") == "update_preedit"
    ]
    return values[-1] if values else None


def last_commit(result: dict[str, Any]) -> str | None:
    values = [
        action["text"]
        for action in result.get("actions", [])
        if action.get("type") == "commit"
    ]
    return values[-1] if values else None


def has_action(result: dict[str, Any], action_type: str) -> bool:
    return any(
        action.get("type") == action_type for action in result.get("actions", [])
    )


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int((len(ordered) - 1) * fraction))
    return ordered[index]


class EnduranceLoop:
    def __init__(self, client: RpcClient, duration: float, report_path: Path) -> None:
        self.client = client
        self.duration = duration
        self.report_path = report_path
        self.started_wall = dt.datetime.now(dt.timezone.utc)
        self.started = time.monotonic()
        self.stop_requested = False
        self.case_runs = 0
        self.probe_runs = 0
        self.key_events = 0
        self.refreshes = 0
        self.rpc_errors = 0
        self.latencies: list[float] = []
        self.refresh_latencies: list[float] = []
        self.outputs: dict[str, collections.Counter[str]] = {
            case.name: collections.Counter() for case in CASES
        }
        self.anomalies: dict[str, dict[str, Any]] = {}

    def elapsed(self) -> float:
        return time.monotonic() - self.started

    def start_clock(self) -> None:
        self.started_wall = dt.datetime.now(dt.timezone.utc)
        self.started = time.monotonic()

    def add_anomaly(
        self,
        kind: str,
        case: Case,
        raw: str,
        converted: str,
        detail: str,
        severity: str,
    ) -> None:
        signature = json.dumps(
            [kind, case.name, raw, converted, detail], ensure_ascii=False
        )
        event = self.anomalies.get(signature)
        if event is None:
            self.anomalies[signature] = {
                "kind": kind,
                "severity": severity,
                "case": case.name,
                "category": case.category,
                "raw": raw,
                "converted": converted,
                "detail": detail,
                "count": 1,
                "first_elapsed_seconds": round(self.elapsed(), 3),
                "last_elapsed_seconds": round(self.elapsed(), 3),
            }
        else:
            event["count"] += 1
            event["last_elapsed_seconds"] = round(self.elapsed(), 3)

    def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        before = time.monotonic()
        try:
            result = self.client.request(method, params)
        except Exception:
            self.rpc_errors += 1
            raise
        self.latencies.append((time.monotonic() - before) * 1000)
        return result

    def press(self, keysym: int) -> dict[str, Any]:
        result = self.request("process_key", {"keysym": keysym})
        self.key_events += 1
        return result

    def refresh_until_settled(self, case: Case, raw: str) -> str:
        converted = raw
        for _ in range(32):
            before = time.monotonic()
            result = self.request("refresh_live_conversion")
            self.refresh_latencies.append((time.monotonic() - before) * 1000)
            self.refreshes += 1
            converted = last_preedit(result) or converted
            if not result.get("needs_live_refresh", False):
                return converted
        self.add_anomaly(
            "refresh_limit",
            case,
            raw,
            converted,
            "32 deferred refreshes did not settle",
            "critical",
        )
        return converted

    def run_case(self, case: Case) -> None:
        self.request("reset")
        previous_raw = ""
        previous_converted = ""
        final_raw = ""
        final_converted = ""

        for segment_index, segment in enumerate(case.segments):
            first_immediate: str | None = None
            for char_index, char in enumerate(segment):
                result = self.press(ord(char))
                if not result.get("consumed", False):
                    self.add_anomaly(
                        "unconsumed_key",
                        case,
                        final_raw,
                        final_converted,
                        f"segment {segment_index}, char {char_index}: {char!r}",
                        "critical",
                    )
                immediate = last_preedit(result)
                if immediate is not None:
                    final_raw = immediate
                    if first_immediate is None:
                        first_immediate = immediate

            previous_has_pending_romaji = bool(previous_raw) and previous_raw[-1].isascii() and previous_raw[-1].isalpha()
            if previous_raw and first_immediate is not None and not previous_has_pending_romaji:
                if not first_immediate.startswith(previous_raw):
                    self.add_anomaly(
                        "immediate_prefix_changed",
                        case,
                        previous_raw,
                        first_immediate,
                        (
                            "the first key after a live refresh did not preserve the "
                            f"exact raw reading; prior conversion was {previous_converted!r}"
                        ),
                        "critical",
                    )

            final_converted = self.refresh_until_settled(case, final_raw)
            previous_raw = final_raw
            previous_converted = final_converted

        self.outputs[case.name][final_converted] += 1
        self.case_runs += 1
        self.inspect_surface(case, final_raw, final_converted)

    def compose(self, case: Case) -> tuple[str, str]:
        self.request("reset")
        raw = ""
        converted = ""
        for segment in case.segments:
            for char in segment:
                result = self.press(ord(char))
                raw = last_preedit(result) or raw
            converted = self.refresh_until_settled(case, raw)
        return raw, converted

    def probe_commit_matches_surface(self, case: Case) -> None:
        raw, converted = self.compose(case)
        result = self.press(KEY_RETURN)
        committed = last_commit(result)
        status = self.request("status")
        if committed != converted:
            self.add_anomaly(
                "commit_surface_mismatch",
                case,
                raw,
                converted,
                f"visible surface {converted!r}, committed text {committed!r}",
                "critical",
            )
        if status.get("state") != "empty":
            self.add_anomaly(
                "commit_state_not_empty",
                case,
                raw,
                converted,
                f"state after Return: {status.get('state')!r}",
                "critical",
            )
        if not has_action(result, "hide_candidates"):
            self.add_anomaly(
                "commit_kept_candidates",
                case,
                raw,
                converted,
                "Return did not emit hide_candidates",
                "critical",
            )
        self.probe_runs += 1

    def probe_cancel_and_reconvert(self, case: Case) -> None:
        raw, converted = self.compose(case)
        result = self.press(KEY_ESCAPE)
        restored = last_preedit(result)
        status = self.request("status")
        if restored != raw or status.get("state") != "composing":
            self.add_anomaly(
                "cancel_did_not_restore_reading",
                case,
                raw,
                converted,
                f"Escape produced {restored!r} in state {status.get('state')!r}",
                "critical",
            )
            self.probe_runs += 1
            return

        result = self.press(KEY_SPACE)
        status = self.request("status")
        if status.get("state") != "conversion" or not has_action(
            result, "show_candidates"
        ):
            self.add_anomaly(
                "explicit_reconversion_unavailable",
                case,
                raw,
                converted,
                (
                    f"Space produced state {status.get('state')!r}; "
                    f"show_candidates={has_action(result, 'show_candidates')}"
                ),
                "critical",
            )
        result = self.press(KEY_ESCAPE)
        reopened = last_preedit(result)
        status = self.request("status")
        if reopened != raw or status.get("state") != "composing":
            self.add_anomaly(
                "reconversion_cancel_lost_reading",
                case,
                raw,
                converted,
                f"Escape produced {reopened!r} in state {status.get('state')!r}",
                "critical",
            )
        self.probe_runs += 1

    def probe_caret_edit(self, case: Case) -> None:
        raw, converted = self.compose(case)
        result = self.press(KEY_LEFT)
        state = last_preedit_state(result)
        expected_caret = max(0, len(raw) - 1)
        if state != (raw, expected_caret):
            self.add_anomaly(
                "caret_move_changed_reading",
                case,
                raw,
                converted,
                f"Left produced {state!r}, expected {(raw, expected_caret)!r}",
                "critical",
            )
            self.probe_runs += 1
            return

        result = self.press(ord("i"))
        edited = last_preedit_state(result)
        expected = raw[:expected_caret] + "い" + raw[expected_caret:]
        if edited != (expected, expected_caret + 1):
            self.add_anomaly(
                "caret_insert_changed_reading",
                case,
                raw,
                edited[0] if edited else "",
                f"middle insert produced {edited!r}, expected {(expected, expected_caret + 1)!r}",
                "critical",
            )
        self.probe_runs += 1

    def probe_context_isolation(self, case: Case) -> None:
        contexts = (
            ("川に架かる", 5),
            ("食事に使う", 5),
        )
        for context, cursor in contexts:
            self.request("reset")
            self.request(
                "set_surrounding_text",
                {"text": context, "cursor_pos": cursor},
            )
            raw = ""
            for segment in case.segments:
                for char in segment:
                    result = self.press(ord(char))
                    raw = last_preedit(result) or raw
            converted = self.refresh_until_settled(case, raw)
            if context in converted or converted.startswith(context):
                self.add_anomaly(
                    "surrounding_context_leaked",
                    case,
                    raw,
                    converted,
                    f"surrounding text {context!r} appeared in preedit",
                    "critical",
                )
            self.inspect_surface(case, raw, converted)
        self.request("set_surrounding_text", {"text": "", "cursor_pos": 0})
        self.probe_runs += 1

    def run_behavior_probes(self) -> None:
        probes = (
            (self.probe_commit_matches_surface, CASES[0]),
            (self.probe_cancel_and_reconvert, CASES[8]),
            (
                self.probe_caret_edit,
                Case("caret_middle_insert", "editing", ("a", "u")),
            ),
            (
                self.probe_context_isolation,
                Case("context_homophone", "context", ("hashi",)),
            ),
        )
        for probe, case in probes:
            if self.stop_requested or self.elapsed() >= self.duration:
                return
            probe(case)

    def inspect_surface(self, case: Case, raw: str, converted: str) -> None:
        if not converted:
            self.add_anomaly(
                "empty_surface", case, raw, converted, "conversion became empty", "critical"
            )
            return

        raw_digits = "".join(char for char in raw if char.isascii() and char.isdigit())
        converted_digits = "".join(
            char for char in converted if char.isascii() and char.isdigit()
        )
        if raw_digits != converted_digits:
            kind = "untyped_digit" if not raw_digits and converted_digits else "digit_changed"
            self.add_anomaly(
                kind,
                case,
                raw,
                converted,
                f"typed digits {raw_digits!r}, converted digits {converted_digits!r}",
                "critical",
            )

        raw_ascii = {char.lower() for char in raw if char.isascii() and char.isalpha()}
        converted_ascii = {
            char.lower() for char in converted if char.isascii() and char.isalpha()
        }
        if not converted_ascii.issubset(raw_ascii):
            self.add_anomaly(
                "untyped_ascii",
                case,
                raw,
                converted,
                f"new ASCII letters: {sorted(converted_ascii - raw_ascii)!r}",
                "critical",
            )

        controls = [char for char in converted if ord(char) < 32 or 0xE000 <= ord(char) <= 0xF8FF]
        if controls:
            self.add_anomaly(
                "internal_character",
                case,
                raw,
                converted,
                f"control/private-use code points: {[hex(ord(c)) for c in controls]}",
                "critical",
            )

        if len(converted) > max(4, int(len(raw) * 1.6)):
            self.add_anomaly(
                "surface_expansion",
                case,
                raw,
                converted,
                f"surface length {len(converted)} vs reading length {len(raw)}",
                "review",
            )

        if case.expected and converted not in case.expected:
            self.add_anomaly(
                "expected_surface_deviation",
                case,
                raw,
                converted,
                f"curated target: {' / '.join(case.expected)}",
                "review",
            )

        case_outputs = self.outputs.get(case.name)
        if case_outputs is not None and len(case_outputs) > 1:
            self.add_anomaly(
                "nondeterministic_surface",
                case,
                raw,
                converted,
                f"observed surfaces: {sorted(case_outputs)!r}",
                "review",
            )

    def run(self) -> None:
        last_progress = -10.0
        cycle = 0
        while not self.stop_requested and self.elapsed() < self.duration:
            cycle += 1
            for case in CASES:
                if self.stop_requested or self.elapsed() >= self.duration:
                    break
                self.run_case(case)
                if self.elapsed() - last_progress >= 10:
                    last_progress = self.elapsed()
                    critical = sum(
                        1 for event in self.anomalies.values() if event["severity"] == "critical"
                    )
                    print(
                        "PROGRESS "
                        f"elapsed={self.elapsed():.1f}s cycles={cycle} cases={self.case_runs} "
                        f"keys={self.key_events} refreshes={self.refreshes} "
                        f"critical_unique={critical} anomalies_unique={len(self.anomalies)}",
                        flush=True,
                    )
            self.run_behavior_probes()

    def report(self) -> dict[str, Any]:
        elapsed = self.elapsed()
        events = sorted(
            self.anomalies.values(),
            key=lambda event: (
                0 if event["severity"] == "critical" else 1,
                event["kind"],
                event["case"],
            ),
        )
        return {
            "schema_version": 1,
            "started_at": self.started_wall.isoformat(),
            "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "requested_duration_seconds": self.duration,
            "elapsed_seconds": round(elapsed, 3),
            "oss_behavior_references": OSS_BEHAVIOR_REFERENCES,
            "server": str(self.client.server),
            "server_sha256": hashlib.sha256(self.client.server.read_bytes()).hexdigest(),
            "counts": {
                "cases": len(CASES),
                "case_runs": self.case_runs,
                "behavior_probe_runs": self.probe_runs,
                "key_events": self.key_events,
                "refreshes": self.refreshes,
                "rpc_errors": self.rpc_errors,
                "unique_anomalies": len(events),
                "critical_unique_anomalies": sum(
                    1 for event in events if event["severity"] == "critical"
                ),
            },
            "latency_ms": {
                "rpc_median": round(statistics.median(self.latencies), 3)
                if self.latencies
                else 0.0,
                "rpc_p95": round(percentile(self.latencies, 0.95), 3),
                "rpc_p99": round(percentile(self.latencies, 0.99), 3),
                "rpc_max": round(max(self.latencies), 3) if self.latencies else 0.0,
                "refresh_median": round(statistics.median(self.refresh_latencies), 3)
                if self.refresh_latencies
                else 0.0,
                "refresh_p95": round(percentile(self.refresh_latencies, 0.95), 3),
                "refresh_p99": round(percentile(self.refresh_latencies, 0.99), 3),
                "refresh_max": round(max(self.refresh_latencies), 3)
                if self.refresh_latencies
                else 0.0,
            },
            "outputs": {
                case.name: dict(self.outputs[case.name].most_common()) for case in CASES
            },
            "anomalies": events,
        }

    def write_report(self) -> dict[str, Any]:
        report = self.report()
        self.report_path.parent.mkdir(parents=True, exist_ok=True)
        self.report_path.write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        markdown_path = self.report_path.with_suffix(".md")
        lines = [
            "# Karukan conversion behavior improvement report",
            "",
            f"- Duration: {report['elapsed_seconds']:.3f} seconds",
            f"- Cases: {report['counts']['cases']} kinds / {report['counts']['case_runs']} runs",
            f"- OSS-inspired behavior probes: {report['counts']['behavior_probe_runs']} runs",
            f"- Key events: {report['counts']['key_events']}",
            f"- Deferred refreshes: {report['counts']['refreshes']}",
            f"- RPC errors: {report['counts']['rpc_errors']}",
            f"- Unique critical anomalies: {report['counts']['critical_unique_anomalies']}",
            f"- Refresh p99 / max: {report['latency_ms']['refresh_p99']} / {report['latency_ms']['refresh_max']} ms",
            "",
            "## Anomalies",
            "",
        ]
        if report["anomalies"]:
            for event in report["anomalies"]:
                lines.extend(
                    [
                        f"- [{event['severity']}] {event['kind']} / {event['case']} (x{event['count']})",
                        f"  - reading: `{event['raw']}`",
                        f"  - surface: `{event['converted']}`",
                        f"  - detail: {event['detail']}",
                    ]
                )
        else:
            lines.append("- None")
        lines.extend(["", "## OSS behavior references", ""])
        for reference in report["oss_behavior_references"]:
            lines.append(
                f"- {reference['project']}: {reference['behavior']} ({reference['url']})"
            )
        lines.extend(["", "## Observed final surfaces", ""])
        for case in CASES:
            surfaces = report["outputs"][case.name]
            rendered = ", ".join(f"`{text}` x{count}" for text, count in surfaces.items())
            lines.append(f"- {case.name} ({case.category}): {rendered or '(not run)'}")
        markdown_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return report


def wait_for_initialization(client: RpcClient) -> dict[str, Any]:
    init = client.request("init")
    deadline = time.monotonic() + 120
    status = client.request("status")
    while status.get("model_name") == "initializing":
        if time.monotonic() >= deadline:
            raise TimeoutError("model initialization did not finish within 120 seconds")
        time.sleep(0.1)
        status = client.request("status")
    print(
        f"READY protocol={init['protocol_version']} model={status['model_name']}", flush=True
    )
    return status


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--duration-seconds", type=float, default=3000.0)
    parser.add_argument("--server", type=Path, default=DEFAULT_SERVER)
    parser.add_argument(
        "--report-path", type=Path, default=Path("/tmp/karukan-ime-endurance.json")
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.duration_seconds <= 0:
        raise SystemExit("--duration-seconds must be positive")
    if not args.server.is_file() or not os.access(args.server, os.X_OK):
        raise SystemExit(f"server is not executable: {args.server}")

    client = RpcClient(args.server)
    loop = EnduranceLoop(client, args.duration_seconds, args.report_path)

    def request_stop(_signum: int, _frame: Any) -> None:
        loop.stop_requested = True

    signal.signal(signal.SIGINT, request_stop)
    signal.signal(signal.SIGTERM, request_stop)
    try:
        wait_for_initialization(client)
        loop.start_clock()
        loop.run()
    finally:
        report = loop.write_report()
        client.close()
    print(
        "DONE "
        f"elapsed={report['elapsed_seconds']:.3f}s cases={report['counts']['case_runs']} "
        f"keys={report['counts']['key_events']} critical_unique="
        f"{report['counts']['critical_unique_anomalies']} report={args.report_path}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
