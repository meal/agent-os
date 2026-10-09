"""Protected acceptance check: python3 check_duration.py <repo_root>"""
import sys
from pathlib import Path

CASES = [
    ("seconds", "90s", 90),
    ("minutes", "5m", 300),
    ("hours and minutes", "1h30m", 5400),
    ("days", "2d", 172800),
    ("days and hours with a space", "1d 2h", 93600),
    ("surrounding spaces", " 45s ", 45),
    ("bare number is seconds", "120", 120),
    ("upper-case unit", "1H", 3600),
    ("invalid text raises ValueError", "invalid", ValueError),
    ("empty text raises ValueError", "", ValueError),
]


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_duration.py <repo_root>")
        return 2
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(Path(sys.argv[1]) / "src"))
    from duration import parse_duration  # noqa: E402

    failed = 0
    for name, text, expected in CASES:
        try:
            got = parse_duration(text)
        except Exception as exc:  # any crash is a failure unless it is the expected error
            got = type(exc)
        if got != expected:
            failed += 1
            print(f"FAIL {name}: expected {expected!r}, got {got!r}")
    print(f"{len(CASES) - failed}/{len(CASES)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
