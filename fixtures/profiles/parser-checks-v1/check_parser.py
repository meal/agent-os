"""Protected acceptance check: python3 check_parser.py <repo_root>"""
import sys
from pathlib import Path

CASES = [
    ("simple pair", "a=1", {"a": "1"}),
    ("spaces around equals", "a = 1", {"a": "1"}),
    ("tabs and padding", "\t a \t=\t 1 \t", {"a": "1"}),
    ("multiple lines", "a = 1\nb = 2", {"a": "1", "b": "2"}),
    ("blank lines", "a = 1\n\n   \nb = 2\n", {"a": "1", "b": "2"}),
    ("comment lines", "# c\na = 1\n# d", {"a": "1"}),
    ("indented comment", "  # c\na = 1", {"a": "1"}),
    ("equals inside value", "a = b=c", {"a": "b=c"}),
    ("empty value", "a =", {"a": ""}),
    ("empty value with space", "a = ", {"a": ""}),
]


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_parser.py <repo_root>")
        return 2
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(Path(sys.argv[1]) / "src"))
    from parser import parse_kv  # noqa: E402

    failed = 0
    for name, text, expected in CASES:
        try:
            got = parse_kv(text)
        except Exception as exc:  # any crash is a failure
            got = f"raised {exc!r}"
        if got != expected:
            failed += 1
            print(f"FAIL {name}: expected {expected!r}, got {got!r}")
    print(f"{len(CASES) - failed}/{len(CASES)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
