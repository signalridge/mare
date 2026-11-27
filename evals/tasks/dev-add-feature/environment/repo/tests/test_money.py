from shopcart import format_cents


def test_format_cents():
    assert format_cents(0) == "$0.00"
    assert format_cents(5) == "$0.05"
    assert format_cents(1234) == "$12.34"
    assert format_cents(-50) == "-$0.50"
