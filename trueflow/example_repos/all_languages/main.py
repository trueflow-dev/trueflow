from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Protocol

class _FakePytest:
    def __getattr__(self, name: str) -> "_FakePytest":
        return self

    def __call__(self, *args, **kwargs) -> "_FakePytest":
        return self


pytest = _FakePytest()

def _custom_decorator(func):
    return func


def another_decorator(func):
    return func


def _decorate(*_args, **_kwargs):
    return _custom_decorator


MAX_RETRIES = 3


class Processor(Protocol):
    def process(self, values: list[int]) -> list[int]:
        ...


@dataclass(frozen=True)
class Config:
    name: str
    threshold: int


@dataclass(frozen=True)
class Multiplier(Processor):
    factor: int

    def process(self, values: list[int]) -> list[int]:
        mapper: Callable[[int], int] = lambda value: value * self.factor
        output: list[int] = []
        for value in values:
            output.append(mapper(value))
        return output


def collect_until(limit: int) -> list[int]:
    values: list[int] = []
    current = 0
    while current < limit:
        values.append(current)
        current += 1
    return values


@pytest.mark.slow
@_custom_decorator
@another_decorator
@_decorate("test")
@_decorate("value")
def test_collect_until() -> None:
    assert collect_until(2) == [0, 1]


def main() -> None:
    config = Config(name="sample", threshold=4)
    processor = Multiplier(factor=2)
    values = collect_until(config.threshold)
    processed = processor.process(values)

    for attempt in range(MAX_RETRIES):
        print(f"attempt {attempt}")

    print(f"{config.name}: {processed}")


if __name__ == "__main__":
    main()
