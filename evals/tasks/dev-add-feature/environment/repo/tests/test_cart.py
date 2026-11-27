import pytest

from shopcart import Cart, Catalog, InvalidQuantity, UnknownProduct


@pytest.fixture
def cart():
    return Cart(Catalog({"apple": 120, "bread": 350, "cheese": 899}))


def test_empty_cart(cart):
    assert cart.lines() == []
    assert cart.subtotal_cents() == 0
    assert cart.total_cents() == 0


def test_add_merges_lines(cart):
    cart.add("apple", 2)
    cart.add("apple")
    cart.add("bread")
    assert [(l.sku, l.qty) for l in cart.lines()] == [("apple", 3), ("bread", 1)]
    assert cart.item_count() == 4
    assert cart.total_cents() == 3 * 120 + 350


def test_add_unknown_sku(cart):
    with pytest.raises(UnknownProduct):
        cart.add("caviar")


@pytest.mark.parametrize("qty", [0, -1, 1.5, True])
def test_add_bad_quantity(cart, qty):
    with pytest.raises(InvalidQuantity):
        cart.add("apple", qty)


def test_remove_some_and_all(cart):
    cart.add("cheese", 3)
    cart.remove("cheese", 1)
    assert cart.lines()[0].qty == 2
    cart.remove("cheese")
    assert cart.lines() == []
    with pytest.raises(UnknownProduct):
        cart.remove("cheese")


def test_summary(cart):
    cart.add("bread", 2)
    s = cart.summary()
    assert s["lines"] == [{"sku": "bread", "qty": 2, "unit_cents": 350, "total_cents": 700}]
    assert s["item_count"] == 2
    assert s["subtotal_cents"] == 700
    assert s["total_cents"] == 700
    assert s["total"] == "$7.00"
