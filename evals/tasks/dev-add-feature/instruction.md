The current directory (`/app`) is `shopcart`, a small Python shopping-cart library (see `README.md`; run its tests with `python -m pytest`). Add coupon support to it, with tests.

Behavior:

- `Cart.apply_coupon(code)` applies a coupon. Codes are case-insensitive and surrounding whitespace is ignored. A cart holds at most one coupon: applying a new one replaces the previous one. An unknown code raises `InvalidCoupon` and leaves the current coupon unchanged.
- `Cart.remove_coupon()` removes the coupon (no error when there is none).
- `Cart.coupon` is the applied code in upper case, or `None`.
- `Cart.discount_cents()` is the current discount; `Cart.total_cents()` becomes `subtotal_cents() - discount_cents()`. The discount is recomputed from the current cart contents every time, and never exceeds the subtotal.
- The coupons:
  - `SAVE10`: 10% off the subtotal, rounded down to whole cents;
  - `SAVE25`: 25% off the subtotal, rounded down to whole cents;
  - `FIVEOFF`: 500 cents off when the subtotal is at least 2000 cents, otherwise no discount (the coupon stays applied and starts counting once the subtotal reaches 2000).
- `Cart.summary()` gains two keys: `"coupon"` (same value as `Cart.coupon`) and `"discount_cents"`; `"total_cents"` and `"total"` reflect the discount. Keep every existing key.
- `InvalidCoupon` is a new exception, a subclass of `CartError`, importable from `shopcart` (as are all existing names).

Add your tests in a new file `tests/test_coupons.py` (pytest, at least 5 test functions). Do not change the existing tests; they must still pass. Run the whole suite before you finish.
