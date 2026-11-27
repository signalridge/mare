#!/bin/bash
# reference solution (Harbor oracle agent): validates the verifier, never shown to mare
set -e
cd /app
cat >> shopcart/errors.py <<'EOF'


class InvalidCoupon(CartError):
    """The coupon code is not known."""
EOF
cat > shopcart/coupons.py <<'EOF'
def _percent(p):
    return lambda subtotal: subtotal * p // 100


def _flat(amount, minimum):
    return lambda subtotal: amount if subtotal >= minimum else 0


COUPONS = {"SAVE10": _percent(10), "SAVE25": _percent(25), "FIVEOFF": _flat(500, 2000)}
EOF
python3 - <<'EOF'
import re
p = "shopcart/cart.py"
s = open(p).read()
s = s.replace("from .errors import InvalidQuantity, UnknownProduct", "from .coupons import COUPONS\nfrom .errors import InvalidCoupon, InvalidQuantity, UnknownProduct")
s = s.replace("        self._lines: dict[str, Line] = {}\n", "        self._lines: dict[str, Line] = {}\n        self.coupon: str | None = None\n")
s = s.replace("""    def total_cents(self) -> int:
        return self.subtotal_cents()
""", """    def apply_coupon(self, code: str) -> None:
        key = code.strip().upper() if isinstance(code, str) else ""
        if key not in COUPONS:
            raise InvalidCoupon(code)
        self.coupon = key

    def remove_coupon(self) -> None:
        self.coupon = None

    def discount_cents(self) -> int:
        sub = self.subtotal_cents()
        return min(sub, COUPONS[self.coupon](sub)) if self.coupon else 0

    def total_cents(self) -> int:
        return self.subtotal_cents() - self.discount_cents()
""")
s = s.replace("""            "subtotal_cents": self.subtotal_cents(),
""", """            "subtotal_cents": self.subtotal_cents(),
            "coupon": self.coupon,
            "discount_cents": self.discount_cents(),
""")
open(p, "w").write(s)
p = "shopcart/__init__.py"
s = open(p).read().replace("InvalidQuantity\n", "InvalidQuantity, InvalidCoupon\n").replace('    "InvalidQuantity",\n', '    "InvalidQuantity",\n    "InvalidCoupon",\n')
open(p, "w").write(s)
EOF
cat > tests/test_coupons.py <<'EOF'
import pytest
from shopcart import Cart, Catalog, InvalidCoupon


@pytest.fixture
def cart():
    return Cart(Catalog({"a": 899, "b": 2000}))


def test_save10(cart):
    cart.add("a"); cart.apply_coupon("save10")
    assert cart.total_cents() == 810


def test_save25(cart):
    cart.add("b"); cart.apply_coupon("SAVE25")
    assert cart.discount_cents() == 500


def test_fiveoff(cart):
    cart.add("a"); cart.apply_coupon("FIVEOFF")
    assert cart.discount_cents() == 0
    cart.add("b")
    assert cart.discount_cents() == 500


def test_invalid(cart):
    with pytest.raises(InvalidCoupon):
        cart.apply_coupon("nope")
    assert cart.coupon is None


def test_summary(cart):
    cart.add("a"); cart.apply_coupon("SAVE10")
    s = cart.summary()
    assert (s["coupon"], s["discount_cents"], s["total"]) == ("SAVE10", 89, "$8.10")
EOF
