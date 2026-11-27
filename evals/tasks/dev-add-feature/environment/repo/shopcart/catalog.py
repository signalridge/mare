from .errors import UnknownProduct


class Catalog:
    """SKU -> unit price in cents."""

    def __init__(self, prices: dict[str, int]):
        for sku, price in prices.items():
            if not isinstance(price, int) or price < 0:
                raise ValueError(f"bad price for {sku!r}: {price!r}")
        self._prices = dict(prices)

    def price(self, sku: str) -> int:
        try:
            return self._prices[sku]
        except KeyError:
            raise UnknownProduct(sku) from None

    def __contains__(self, sku: str) -> bool:
        return sku in self._prices
