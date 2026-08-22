(
    "module docstring position"
    + "?"
)


def function():
    (
        "function docstring position"
        + "?"
    )
    return 1


class Class:
    (
        "class docstring position"
        + "?"
    )


# Not a docstring position, so the fix stays safe.
(
    "after another statement"
    + "?"
)
