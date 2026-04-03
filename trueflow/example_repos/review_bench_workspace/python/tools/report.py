from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


@dataclass(frozen=True)
class ReviewRow:
    path: str
    blocks: int
    reviewed: int

    @property
    def coverage(self) -> float:
        if self.blocks == 0:
            return 1.0
        return self.reviewed / self.blocks


def load_rows(path: Path) -> list[ReviewRow]:
    rows: list[ReviewRow] = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        file_path, blocks, reviewed = line.split(",")
        rows.append(ReviewRow(path=file_path, blocks=int(blocks), reviewed=int(reviewed)))
    return rows


def summarize(rows: Iterable[ReviewRow]) -> dict[str, float]:
    rows = list(rows)
    total_blocks = sum(row.blocks for row in rows)
    total_reviewed = sum(row.reviewed for row in rows)
    avg_coverage = 1.0 if total_blocks == 0 else total_reviewed / total_blocks
    return {
        "files": float(len(rows)),
        "blocks": float(total_blocks),
        "reviewed": float(total_reviewed),
        "avg_coverage": avg_coverage,
    }


def main() -> None:
    rows = load_rows(Path("review-report.csv"))
    summary = summarize(rows)
    for key, value in summary.items():
        print(f"{key}={value}")


if __name__ == "__main__":
    main()
