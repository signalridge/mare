class CartError(Exception):
    """Base class for every error raised by shopcart."""


class UnknownProduct(CartError):
    """The SKU is not in the catalog (or not in the cart)."""


class InvalidQuantity(CartError):
    """A quantity was not a positive integer."""
