def parse_kv(text: str) -> dict:
    """Parse 'key = value' lines into a dict, skipping blanks and '#' comments."""
    result = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        result[key] = value
    return result
