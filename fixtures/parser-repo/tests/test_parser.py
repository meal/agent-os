import unittest

from src.parser import parse_kv


class ParseKvTest(unittest.TestCase):
    def test_simple_pair(self):
        self.assertEqual(parse_kv("a=1"), {"a": "1"})

    def test_multiple_lines(self):
        self.assertEqual(parse_kv("a=1\nb=2"), {"a": "1", "b": "2"})

    def test_skips_blank_lines(self):
        self.assertEqual(parse_kv("a=1\n\nb=2\n"), {"a": "1", "b": "2"})

    def test_skips_comments(self):
        self.assertEqual(parse_kv("# note\na=1"), {"a": "1"})

    def test_spaces_around_equals(self):
        self.assertEqual(parse_kv("name = alice"), {"name": "alice"})

    def test_surrounding_whitespace(self):
        self.assertEqual(parse_kv("  k  =  v  "), {"k": "v"})


if __name__ == "__main__":
    unittest.main()
