import pytest

import shopcart
from shopcart import Cart, CartError, Catalog


@pytest.fixture
def cart():
    return Cart(Catalog({"apple": 120, "bread": 350, "cheese": 899, "wine": 2000}))


def test_exported():
    assert issubclass(shopcart.InvalidCoupon, CartError)
    for name in ("Cart", "Line", "Catalog", "CartError", "UnknownProduct", "InvalidQuantity", "format_cents"):
        assert hasattr(shopcart, name)


def test_no_coupon_by_default(cart):
    cart.add("bread")
    assert cart.coupon is None
    assert cart.discount_cents() == 0
    assert cart.total_cents() == 350


def test_save10_rounds_down(cart):
    cart.add("cheese")  # 899 -> 89.9 off -> 89
    cart.apply_coupon("SAVE10")
    assert cart.coupon == "SAVE10"
    assert cart.discount_cents() == 89
    assert cart.total_cents() == 810


def test_save25(cart):
    cart.add("apple", 3)  # 360 -> 90
    cart.apply_coupon("SAVE25")
    assert cart.discount_cents() == 90
    assert cart.total_cents() == 270


def test_case_and_whitespace_insensitive(cart):
    cart.add("bread", 2)
    cart.apply_coupon("  save10 ")
    assert cart.coupon == "SAVE10"
    assert cart.discount_cents() == 70


def test_fiveoff_threshold(cart):
    cart.add("cheese", 2)  # 1798
    cart.apply_coupon("fiveoff")
    assert cart.coupon == "FIVEOFF"
    assert cart.discount_cents() == 0
    assert cart.total_cents() == 1798
    cart.add("apple", 2)  # 2038
    assert cart.discount_cents() == 500
    assert cart.total_cents() == 1538
    cart.remove("apple", 1)  # 1918
    assert cart.discount_cents() == 0


def test_fiveoff_exactly_2000(cart):
    cart.add("wine")
    cart.apply_coupon("FIVEOFF")
    assert cart.total_cents() == 1500


def test_recomputed_after_changes(cart):
    cart.apply_coupon("SAVE10")
    assert cart.discount_cents() == 0
    cart.add("bread", 3)  # 1050 -> 105
    assert cart.discount_cents() == 105
    cart.remove("bread")
    assert cart.discount_cents() == 0
    assert cart.total_cents() == 0


def test_replace_coupon(cart):
    cart.add("wine")
    cart.apply_coupon("SAVE10")
    cart.apply_coupon("SAVE25")
    assert cart.coupon == "SAVE25"
    assert cart.discount_cents() == 500


def test_invalid_coupon_keeps_current(cart):
    cart.add("wine")
    cart.apply_coupon("SAVE10")
    with pytest.raises(shopcart.InvalidCoupon):
        cart.apply_coupon("FREESTUFF")
    with pytest.raises(CartError):
        cart.apply_coupon("")
    assert cart.coupon == "SAVE10"
    assert cart.discount_cents() == 200


def test_remove_coupon(cart):
    cart.add("wine")
    cart.remove_coupon()  # no coupon: no error
    cart.apply_coupon("SAVE25")
    cart.remove_coupon()
    assert cart.coupon is None
    assert cart.total_cents() == 2000


def test_summary_keys(cart):
    cart.add("cheese")
    s = cart.summary()
    assert s["coupon"] is None and s["discount_cents"] == 0
    cart.apply_coupon("save10")
    s = cart.summary()
    assert s["coupon"] == "SAVE10"
    assert s["discount_cents"] == 89
    assert s["subtotal_cents"] == 899
    assert s["total_cents"] == 810
    assert s["total"] == "$8.10"
    assert s["item_count"] == 1
    assert s["lines"] == [{"sku": "cheese", "qty": 1, "unit_cents": 899, "total_cents": 899}]


def test_discount_never_exceeds_subtotal():
    cart = Cart(Catalog({"a": 1}))
    cart.add("a")
    cart.apply_coupon("SAVE25")
    assert cart.discount_cents() == 0
    assert cart.total_cents() == 1
