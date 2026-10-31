"""Run transport smoke or caller-owned OCR text checks with an installed almiladoc wheel.

Without --corpus, fill the five-page PDF fixture and print its Markdown. --corpus takes JSON:
{"version": 1, "cases": [{"image": "page.png", "sha256": "<64 lowercase hex>",
"text": ["literal paragraph", "İstanbul"], "headings": [{"level": 1, "text": "Title"}]}]}.
Paths stay within the manifest directory. Supply real, reviewed PNGs; no quality corpus is bundled.
Text snippets must occur in order after NFC/whitespace normalization; headings require their
supplied ATX/Setext level outside code blocks; HTML is refused when checking headings.
Empty text/headings require a blank reply. At least one text case is required.
Corpus mode prints only POST chat/completions (relative to base_url) and fixed statuses, in case
order, and runs in an owned worker killed at --timeout. It does not certify model accuracy.
"""

import argparse
import hashlib
import json
import re
import struct
import subprocess
import sys
import time
import unicodedata
from pathlib import Path
from typing import NoReturn
from urllib.parse import urlsplit

import almiladoc

FIXTURE = Path(__file__).resolve().parents[1] / "tests/fixtures/pdf/handmade-five-page.pdf"


def settle(
    parsed: almiladoc.Parsed, request: almiladoc.OcrRequest, outcome: str | almiladoc.OcrFailure
) -> str:
    if isinstance(outcome, almiladoc.OcrFailure):
        if outcome.attempted:
            parsed.fail_gap(request.gap)
        return f"{'failed' if outcome.attempted else 'unattempted'}, status {outcome.status}"
    try:
        parsed.fill_gap(request.gap, outcome)
    except almiladoc.GapRefused as refused:
        parsed.fail_gap(request.gap)
        return f"refused, past {refused.limit}"
    return f"filled, {len(outcome)} characters"


MAX_MANIFEST = 64 << 10
MAX_IMAGE = 4 << 20
MAX_TOTAL = 16 << 20
MAX_TEXT = 64 << 10
MAX_CASES = 8
STATUSES = re.compile(
    r"(?:match|text-mismatch|heading-mismatch|blank-mismatch|oversized-reply|"
    r"invalid-corpus|invalid-config|error|timeout|failed-(?:none|[0-9]{3})|"
    r"unattempted-(?:none|[0-9]{3}))\Z"
)
WORKER = (
    "import runpy,sys; "
    "raise SystemExit(runpy.run_path(sys.argv[1])['corpus_worker'](sys.argv[2:]))"
)


def normalize(text: str) -> str:
    return " ".join(unicodedata.normalize("NFC", text).split())


def bounded_file(path: Path, ceiling: int) -> bytes:
    if not path.is_file():
        raise ValueError
    with path.open("rb") as source:
        data = source.read(ceiling + 1)
    if len(data) > ceiling:
        raise ValueError
    return data


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = dict(pairs)
    if len(result) != len(pairs):
        raise ValueError
    return result


def strings(value: object, ceiling: int) -> list[str]:
    if not isinstance(value, list) or len(value) > 16:
        raise ValueError
    if any(
        not isinstance(item, str) or not normalize(item) or len(item) > ceiling for item in value
    ):
        raise ValueError
    return value


def corpus(path: Path) -> list[tuple[bytes, list[str], list[tuple[int, str]]]]:
    manifest = json.loads(bounded_file(path, MAX_MANIFEST), object_pairs_hook=unique_object)
    if not isinstance(manifest, dict) or set(manifest) != {"version", "cases"}:
        raise ValueError
    cases = manifest["cases"]
    if type(manifest["version"]) is not int or manifest["version"] != 1:
        raise ValueError
    if not isinstance(cases, list) or not 1 <= len(cases) <= MAX_CASES:
        raise ValueError
    root, total, loaded = path.resolve().parent, 0, []
    for case in cases:
        if not isinstance(case, dict) or set(case) != {"image", "sha256", "text", "headings"}:
            raise ValueError
        if not isinstance(case["image"], str) or len(case["image"]) > 1024:
            raise ValueError
        name = Path(case["image"])
        image_path = (root / name).resolve()
        if (
            name.is_absolute()
            or name.suffix.lower() != ".png"
            or not image_path.is_relative_to(root)
        ):
            raise ValueError
        image = bounded_file(image_path, MAX_IMAGE)
        total += len(image)
        if total > MAX_TOTAL or not isinstance(case["sha256"], str):
            raise ValueError
        if hashlib.sha256(image).hexdigest() != case["sha256"]:
            raise ValueError
        if len(image) < 33 or image[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR":
            raise ValueError
        width, height = struct.unpack(">II", image[16:24])
        if min(width, height) < 32 or width * height > 1_048_576:
            raise ValueError
        text, headings = strings(case["text"], 4096), case["headings"]
        if not isinstance(headings, list) or len(headings) > 16:
            raise ValueError
        checked = []
        for heading in headings:
            if not isinstance(heading, dict) or set(heading) != {"level", "text"}:
                raise ValueError
            level = heading["level"]
            if type(level) is not int or not 1 <= level <= 6:
                raise ValueError
            checked.append((level, strings([heading["text"]], 256)[0]))
        loaded.append((image, text, checked))
    if not any(text for _, text, _ in loaded):
        raise ValueError
    return loaded


def headings_in(markdown: str) -> list[tuple[int, str]]:
    result, previous, fence = [], [], ""
    for source in re.split(r"\r\n?|\n", markdown):
        line = source.strip()
        if fence:
            if not source.expandtabs(4).startswith("    ") and re.fullmatch(
                re.escape(fence[0]) + "{" + str(len(fence)) + r",}\s*", line
            ):
                fence = ""
            continue
        if match := re.match(r" {0,3}(`{3,}|~{3,})", source):
            fence, previous = match[1], []
            continue
        if source.expandtabs(4).startswith("    "):
            previous = []
            continue
        if match := re.fullmatch(r" {0,3}(#{1,6})[ \t]+(.+?)(?:[ \t]+#+)?[ \t]*", source):
            result.append((len(match[1]), normalize(match[2])))
            previous = []
            continue
        if previous and (match := re.fullmatch(r" {0,3}(=+|-+)[ \t]*", source)):
            result.append((1 if match[1][0] == "=" else 2, normalize(" ".join(previous))))
            previous = []
            continue
        if line:
            previous.append(line)
        else:
            previous = []
    return result


def judge(reply: str, text: list[str], headings: list[tuple[int, str]]) -> str:
    if len(reply.encode("utf-8")) > MAX_TEXT:
        return "oversized-reply"
    if not text and not headings:
        return "match" if not reply.strip() else "blank-mismatch"
    normalized, cursor = normalize(reply), 0
    for snippet in text:
        at = normalized.find(normalize(snippet), cursor)
        if at < 0:
            return "text-mismatch"
        cursor = at + len(normalize(snippet))
    if headings and re.search(r"(?m)^ {0,3}<(?:!--|/?[A-Za-z])", reply):
        return "heading-mismatch"
    actual, cursor = headings_in(reply), 0
    for level, title in headings:
        try:
            cursor = actual.index((level, normalize(title)), cursor) + 1
        except ValueError:
            return "heading-mismatch"
    return "match"


def recognize_corpus(args: argparse.Namespace) -> list[str]:
    try:
        cases = corpus(args.corpus)
    except (OSError, ValueError, RecursionError):
        return ["invalid-corpus"]
    key = bounded_file(args.key_file, 8192).decode("utf-8").strip() if args.key_file else None
    engine = almiladoc.OcrHttp(
        args.base_url,
        args.model,
        api_key=key,
        concurrency=1,
        retries=0,
        temperature=0.0,
        max_tokens=args.max_tokens or 7000,
    )
    statuses = []
    for index, (image, text, headings) in enumerate(cases):
        request = almiladoc.OcrRequest(
            (None, None, index + 1, None, None, []), image, "image/png", MAX_TEXT
        )
        outcomes = engine.recognize([request])
        if len(outcomes) != 1:
            return ["error"]
        outcome = outcomes[0]
        if isinstance(outcome, almiladoc.OcrFailure):
            status = "none" if outcome.status is None else str(outcome.status)
            statuses.append(f"{'failed' if outcome.attempted else 'unattempted'}-{status}")
            break
        statuses.append(judge(outcome, text, headings))
    return statuses


class Arguments(argparse.ArgumentParser):
    def error(self, message: str) -> NoReturn:
        raise ValueError


def arguments(argv: list[str] | None = None) -> argparse.Namespace:
    parser = Arguments(prog="ocr_live", description=__doc__.splitlines()[0])
    parser.add_argument("base_url")
    parser.add_argument("model")
    parser.add_argument("--key-file", type=Path, help="a file outside every repository")
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--corpus", type=Path, help="a reviewed, SHA-256-pinned PNG manifest")
    parser.add_argument(
        "--timeout", type=float, default=120, help="corpus worker wall seconds, at most 900"
    )
    parser.add_argument("--max-tokens", type=int)
    args = parser.parse_args(argv)
    if not 0 < args.timeout <= 900 or (
        args.max_tokens is not None and not 1 <= args.max_tokens <= 8192
    ):
        raise ValueError
    if args.corpus and args.concurrency != 1:
        raise ValueError
    return args


def corpus_worker(argv: list[str]) -> int:
    try:
        statuses = recognize_corpus(arguments(argv))
    except almiladoc.ConfigError:
        statuses = ["invalid-config"]
    except Exception:
        statuses = ["error"]
    print(json.dumps(statuses))
    return 0


def corpus_process(args: argparse.Namespace, argv: list[str]) -> int:
    try:
        done = subprocess.run(
            [sys.executable, "-I", "-B", "-c", WORKER, str(Path(__file__).resolve()), *argv],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=args.timeout,
            check=False,
        )
        statuses = (
            json.loads(done.stdout) if done.returncode == 0 and len(done.stdout) <= 1024 else []
        )
        if not isinstance(statuses, list) or not 1 <= len(statuses) <= MAX_CASES:
            raise ValueError
        if any(
            not isinstance(status, str) or not STATUSES.fullmatch(status) for status in statuses
        ):
            raise ValueError
    except subprocess.TimeoutExpired:
        statuses = ["timeout"]
    except (OSError, ValueError):
        statuses = ["error"]
    for status in statuses:
        print(f"POST chat/completions status={status}", file=sys.stderr)
    return 0 if all(status == "match" for status in statuses) else 1


def smoke(args: argparse.Namespace) -> int:
    key = bounded_file(args.key_file, 8192).decode("utf-8").strip() if args.key_file else None
    engine = almiladoc.OcrHttp(
        args.base_url,
        args.model,
        api_key=key,
        concurrency=args.concurrency,
        max_tokens=args.max_tokens or 8192,
    )
    config = almiladoc.Config(on_loss=almiladoc.OnLoss.REPORT)
    parsed = almiladoc.Converter(config).parse(FIXTURE.read_bytes())
    requests = parsed.ocr_requests()
    path = urlsplit(args.base_url).path.rstrip("/") + "/chat/completions"
    print(f"POST {path}: {len(requests)} gaps", file=sys.stderr)
    began = time.monotonic()
    outcomes = engine.recognize(requests)
    read = [settle(parsed, *pair) for pair in zip(requests, outcomes, strict=True)]
    for request, outcome in zip(requests, read, strict=True):
        print(f"page {request.gap[3]}: {outcome}", file=sys.stderr)
    print(f"{time.monotonic() - began:.1f} s", file=sys.stderr)
    print(parsed.render().markdown, end="")
    return 0 if all(outcome.startswith("filled") for outcome in read) else 1


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    try:
        args = arguments(argv)
        return corpus_process(args, argv) if args.corpus else smoke(args)
    except Exception:
        print("POST chat/completions status=error", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
