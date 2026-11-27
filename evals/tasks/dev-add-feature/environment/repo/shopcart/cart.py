from dataclasses import dataclass

from .catalog import Catalog
from .errors import InvalidQuantity, UnknownProduct
from .money import format_cents


@dataclass
class Line:
    sku: str
    unit_cents: int
    qty: int

    @property
    def total_cents(self) -> int:
        return self.unit_cents * self.qty


class Cart:
    def __init__(self, catalog: Catalog):
        self.catalog = catalog
        self._lines: dict[str, Line] = {}

    def add(self, sku: str, qty: int = 1) -> None:
        if not isinstance(qty, int) or isinstance(qty, bool) or qty <= 0:
            raise InvalidQuantity(qty)
        price = self.catalog.price(sku)
        if sku in self._lines:
            self._lines[sku].qty += qty
        else:
            self._lines[sku] = Line(sku, price, qty)

    def remove(self, sku: str, qty: int | None = None) -> None:
        """Remove `qty` units of `sku` (all of them when qty is None)."""
        if sku not in self._lines:
            raise UnknownProduct(sku)
        if qty is None or qty >= self._lines[sku].qty:
            del self._lines[sku]
            return
        if not isinstance(qty, int) or isinstance(qty, bool) or qty <= 0:
            raise InvalidQuantity(qty)
        self._lines[sku].qty -= qty

    def lines(self) -> list[Line]:
        return list(self._lines.values())

    def item_count(self) -> int:
        return sum(l.qty for l in self._lines.values())

    def subtotal_cents(self) -> int:
        return sum(l.total_cents for l in self._lines.values())

    def total_cents(self) -> int:
        return self.subtotal_cents()

    def summary(self) -> dict:
        return {
            "lines": [
                {"sku": l.sku, "qty": l.qty, "unit_cents": l.unit_cents, "total_cents": l.total_cents}
                for l in self._lines.values()
            ],
            "item_count": self.item_count(),
            "subtotal_cents": self.subtotal_cents(),
            "total_cents": self.total_cents(),
            "total": format_cents(self.total_cents()),
        }
