import pytest


def test_ok():
    try:
        something()
    except Exception as e:
        something_else()

    with pytest.raises(ZeroDivisionError) as e:
        1 / 0
    assert e.value.message


def test_error():
    try:
        something()
    except Exception as e:
        assert e.message, "blah blah"


# https://github.com/astral-sh/ruff/issues/27870
# One diagnostic per assertion, even when the exception is named more than once.
def test_error_repeated_reference():
    try:
        something()
    except ZeroDivisionError as e:
        assert len(e.args) == 1, e.args
