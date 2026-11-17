"""Every definition the Python extractor makes, once."""

import functools
import unittest

import pytest


def clamp(value, low, high):
    """A docstring is a string, one token."""
    if value < low:
        return low
    elif value > high:
        return high
    return value


def bound(x, lo, hi):
    """The same shape as `clamp`: names, literals and comments differ."""
    # A comment is not a token.
    if x < lo:
        return lo
    elif x > hi:
        return hi
    return x


def signature(a, /, b: int, c=1, *args, d, e: str = "e", **kwargs) -> None:
    pass


def keyword_only(*, a, b):
    return lambda: a + b


async def fetch(session, urls):
    async with session:
        for url in urls:
            try:
                yield await session.get(url)
            except TimeoutError:
                continue
            except (KeyError, ValueError) as error:
                raise RuntimeError(f"{url}: {error!r}") from error


def describe(shape):
    match shape:
        case {"kind": "circle", "radius": r} if r > 0:
            return "circle"
        case [x, y]:
            return "pair" if x and y or not x else "none"
        case _:
            return "unknown"


def outer(items):
    def inner(item):
        def innermost():
            return item

        return innermost()

    evens = [inner(i) for i in items if i % 2 == 0]
    return sorted(evens, key=lambda e: (lambda f: -f)(e))


class Point:
    origin = staticmethod(lambda: Point(0, 0))

    def __init__(self, x, y):
        self.x, self.y = x, y

    @property
    def norm(self):
        return (self.x**2 + self.y**2) ** 0.5

    @staticmethod
    @functools.cache
    def parse(text):
        x, y = text.split(",")
        return Point(int(x), int(y))

    class Builder:
        def build(self):
            return Point(0, 0)


def make():
    class Local:
        def method(self):
            return self

    return Local


@functools.lru_cache(maxsize=None, typed=lambda: False)
def memoized(n):
    return n


if __debug__:
    def speed(): return 1
else:
    def speed(): return 2


def test_clamp():
    assert all(clamp(v, 0, 1) <= 1 for v in range(3))


@pytest.fixture
def point():
    return Point(1, 2)


@pytest.mark.parametrize("text", ["1,2"])
@functools.wraps(make)
def parses(text):
    assert Point.parse(text)


class TestPoint:
    def helper(self):
        return lambda: Point(0, 0)


class PointTests(unittest.TestCase):
    def setUp(self):
        self.point = Point(3, 4)
