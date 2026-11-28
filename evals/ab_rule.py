"""Statistical A/B comparison rule for Harbor scenario evaluations.

Follows Dim's `xtask/src/rule.rs`: one comparison is the candidate arm's loss
against the baseline arm on one rate. A comparison rejects (regression) when
its loss strictly exceeds ``DELTA`` (0.05, decided in integers) and a one-sided
mid-p Fisher exact test rejects under Holm step-down adjustment at family-wise
``ALPHA`` (0.10). The three-valued verdict is ``fail`` when any comparison
rejects, ``pass`` when none rejects, every comparison has counted runs in both
arms, and every arm has at least ``floor`` judged runs on every scenario, and
``inconclusive`` otherwise. Standard library only.
"""

from __future__ import annotations

import dataclasses
import enum
import math

DELTA: float = 0.05
ALPHA: float = 0.10
FLOOR: int = 20


@dataclasses.dataclass(frozen=True)
class Counts:
    outcome: int = 0
    total: int = 0

    def __post_init__(self) -> None:
        if self.outcome < 0 or self.total < 0 or self.outcome > self.total:
            raise ValueError(f"invalid counts: {self.outcome} of {self.total}")

    def rate(self) -> float | None:
        return (self.outcome / self.total) if self.total > 0 else None


class Worse(str, enum.Enum):
    LOWER = "lower"
    HIGHER = "higher"


@dataclasses.dataclass(frozen=True)
class Comparison:
    name: str
    worse: Worse
    baseline: Counts
    candidate: Counts


@dataclasses.dataclass
class Row:
    name: str
    worse: Worse
    baseline: Counts
    candidate: Counts
    loss: float | None
    p: float
    threshold: float = 0.0
    rejected: bool = False

    def line(self) -> str:
        def cell(counts: Counts) -> str:
            rate = counts.rate()
            return f"{counts.outcome}/{counts.total} ({rate:.3f})" if rate is not None else "-"

        loss_text = f"{self.loss:.3f}" if self.loss is not None else "-"
        return (
            f"| {self.name} | {cell(self.baseline)} | {cell(self.candidate)} "
            f"| {loss_text} | {format_probability(self.p)} | {self.threshold:.4f} "
            f"| {'yes' if self.rejected else 'no'} |"
        )


COMPARISON_HEADER = (
    "| comparison | baseline | candidate | loss | p | Holm threshold | rejected |\n"
    "| --- | --- | --- | --- | --- | --- | --- |"
)


@dataclasses.dataclass(frozen=True)
class RuleResult:
    rows: list[Row]
    verdict: str  # "pass" | "fail" | "inconclusive"
    floor: int


def exceeds_delta(comp: Comparison) -> bool:
    """Decide whether loss strictly exceeds 1/20 using exact integer arithmetic."""
    if comp.worse is Worse.LOWER:
        a, an = comp.baseline.outcome, comp.baseline.total
        b, bn = comp.candidate.outcome, comp.candidate.total
    else:
        a, an = comp.candidate.outcome, comp.candidate.total
        b, bn = comp.baseline.outcome, comp.baseline.total
    if an == 0 or bn == 0:
        return False
    # a/an - b/bn > 1/20  <=>  20 * (a * bn - b * an) > an * bn
    return 20 * a * bn > 20 * b * an + an * bn


def _log_choose(n: int, k: int) -> float:
    return math.lgamma(n + 1) - math.lgamma(k + 1) - math.lgamma(n - k + 1)


def mid_p_fisher(low: Counts, high: Counts) -> float:
    """One-sided mid-p Fisher exact test: P(X < observed) + 0.5 * P(X == observed)."""
    draws = low.total
    population = low.total + high.total
    successes = low.outcome + high.outcome
    observed = low.outcome
    denom = _log_choose(population, draws)

    def prob(x: int) -> float:
        return math.exp(_log_choose(successes, x) + _log_choose(population - successes, draws - x) - denom)

    lowest = max(0, draws - (population - successes))
    p = 0.5 * prob(observed)
    for x in range(lowest, observed):
        p += prob(x)
    return min(p, 1.0)


def _row_of(comp: Comparison) -> Row:
    b_rate = comp.baseline.rate()
    c_rate = comp.candidate.rate()
    if b_rate is not None and c_rate is not None:
        loss = (b_rate - c_rate) if comp.worse is Worse.LOWER else (c_rate - b_rate)
    else:
        loss = None
    if loss is not None and exceeds_delta(comp):
        if comp.worse is Worse.LOWER:
            p = mid_p_fisher(comp.candidate, comp.baseline)
        else:
            p = mid_p_fisher(comp.baseline, comp.candidate)
    else:
        p = 1.0
    return Row(
        name=comp.name,
        worse=comp.worse,
        baseline=comp.baseline,
        candidate=comp.candidate,
        loss=loss,
        p=p,
    )


def _holm(rows: list[Row], alpha: float = ALPHA) -> None:
    m = len(rows)
    order = sorted(range(m), key=lambda idx: rows[idx].p)
    stopped = False
    for rank, idx in enumerate(order):
        threshold = alpha / (m - rank)
        rows[idx].threshold = threshold
        rejected = (not stopped) and (rows[idx].p <= threshold)
        rows[idx].rejected = rejected
        if not rejected:
            stopped = True


def judge(comparisons: list[Comparison], min_judged: int, *, floor: int = FLOOR) -> RuleResult:
    """Apply the fixed-family Holm-adjusted regression rule."""
    rows = [_row_of(comp) for comp in comparisons]
    _holm(rows)
    incomplete = any(row.loss is None for row in rows)
    if any(row.rejected for row in rows):
        verdict = "fail"
    elif min_judged >= floor and not incomplete:
        verdict = "pass"
    else:
        verdict = "inconclusive"
    return RuleResult(rows=rows, verdict=verdict, floor=floor)


def format_probability(p: float) -> str:
    if p >= 1.0:
        return "1"
    if p >= 0.0001:
        return f"{p:.4f}"
    return f"{p:.2e}"


def verdict_line(result: RuleResult, min_judged: int, *, case: str = "scenario", subject: str = "candidate") -> str:
    if result.verdict == "fail":
        parts = []
        for row in result.rows:
            if not row.rejected:
                continue
            direction = "falls" if row.worse is Worse.LOWER else "rises"
            b_rate = row.baseline.rate() or 0.0
            c_rate = row.candidate.rate() or 0.0
            loss_text = f"{row.loss:.3f}" if row.loss is not None else "-"
            parts.append(
                f"{row.name} {direction} from {b_rate:.3f} to {c_rate:.3f} "
                f"(loss {loss_text}, p {format_probability(row.p)}, Holm threshold {row.threshold:.4f})"
            )
        return f"{subject} fails: {'; '.join(parts)}."
    if result.verdict == "pass":
        return (
            f"{subject} passes: no comparison is a regression, and every arm has at least "
            f"{result.floor} judged runs of every {case}."
        )
    reasons = [f"{row.name} has no counted run in an arm" for row in result.rows if row.loss is None]
    if min_judged < result.floor:
        reasons.append(f"the fewest judged runs on a {case} are {min_judged}, under the floor of {result.floor}")
    return f"{subject} is inconclusive: no comparison is a regression, but {', and '.join(reasons)}."


def format_table(rows: list[Row]) -> str:
    return "\n".join([COMPARISON_HEADER, *(row.line() for row in rows)])
