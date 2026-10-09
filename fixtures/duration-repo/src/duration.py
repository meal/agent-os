UNITS = {"s": 1, "m": 60, "h": 3600}


def parse_duration(text: str) -> int:
    """Parse durations like '90s', '5m', '1h30m' or '2d 4h' into seconds."""
    total = 0
    number = ""
    for ch in text:
        if ch.isdigit():
            number += ch
        else:
            total += int(number) * UNITS[ch]
            number = ""
    return total
