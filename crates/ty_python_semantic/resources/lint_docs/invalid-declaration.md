## What it does

Checks for declarations where the inferred type of an existing symbol is not
[assignable to] its post-hoc declared type, or where an instance attribute is declared
without an available slot in a class that does not have an instance dictionary.

## Why is this bad?

Such declarations break the rules of the type system and
weaken a type checker's ability to accurately reason about your code.

## Examples

```python
a = 1
a: str  # error
```

An instance attribute also cannot be declared if the class's slots do not provide
storage for it:

```python
class Slotted:
    __slots__ = ("value",)
    value: int
    other: int  # error
```

[assignable to]: https://typing.python.org/en/latest/spec/glossary.html#term-assignable
