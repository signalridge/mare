# shopcart

A tiny shopping-cart library. Money is always an integer number of cents.

```python
from shopcart import Cart, Catalog

catalog = Catalog({"apple": 120, "bread": 350})
cart = Cart(catalog)
cart.add("apple", 3)
cart.add("bread")
cart.total_cents()   # 710
cart.summary()       # {"lines": [...], "subtotal_cents": 710, "total_cents": 710, ...}
```

Run the tests with `python -m pytest`.
