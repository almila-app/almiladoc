"""Fill the five-page PDF fixture's gaps through a live OpenAI-compatible vision server.

Run it with a Python that has the almiladoc wheel installed, such as one of `make python`'s
`target/make/python/venv-*`. It logs the endpoint's method and path and each gap's page and
outcome to stderr, never the key or the text read, prints the Markdown, and exits 1 unless every
gap was filled.
"""

import argparse
import sys
import time
from pathlib import Path
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("base_url")
    parser.add_argument("model")
    parser.add_argument("--key-file", type=Path, help="a file outside every repository")
    parser.add_argument("--concurrency", type=int, default=1)
    args = parser.parse_args()
    key = args.key_file.read_text(encoding="utf-8").strip() if args.key_file else None
    engine = almiladoc.OcrHttp(
        args.base_url, args.model, api_key=key, concurrency=args.concurrency
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


if __name__ == "__main__":
    sys.exit(main())
