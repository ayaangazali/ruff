# Instance slots

Classes can declare instance attributes and restrict their instance layout with `__slots__`.

## Slot names declare instance attributes

A slot is a valid instance attribute even when no method assigns to it.

```py
class Slotted:
    __slots__ = ("value",)

reveal_type(Slotted().value)  # revealed: Unknown
Slotted().value = 1
```

## Slots are class descriptors

A slot creates a descriptor on its defining class, and its name is visible when enumerating the
attributes of either the class or an instance.

```py
from types import MemberDescriptorType
from ty_extensions import static_assert
from ty_extensions._internal import has_member

class Slotted:
    __slots__ = ("value",)

reveal_type(Slotted.value)  # revealed: MemberDescriptorType
static_assert(has_member(Slotted, "value"))
static_assert(has_member(Slotted(), "value"))

def accepts_member_descriptor(descriptor: MemberDescriptorType) -> None: ...
def accepts_property(descriptor: property) -> None: ...

accepts_member_descriptor(Slotted.value)
accepts_property(Slotted.value)  # error: [invalid-argument-type]

Slotted.value.fget  # error: [unresolved-attribute]
Slotted.value.setter(lambda instance, value: None)  # error: [unresolved-attribute]

descriptor: MemberDescriptorType = Slotted.value
reveal_type(descriptor.__get__(Slotted(), Slotted))  # revealed: Any
```

## Class dictionaries are separate from instance dictionary slots

An instance dictionary slot must not replace the namespace exposed by its class or subclasses.

```py
class WithDictionary:
    __slots__ = ("value", "__dict__")

class SlottedChild(WithDictionary):
    __slots__ = ()

reveal_type(WithDictionary.__dict__)  # revealed: dict[str, Any]
reveal_type(WithDictionary.__dict__["value"])  # revealed: Any
reveal_type(SlottedChild.__dict__)  # revealed: dict[str, Any]

def inspect_class(cls: type[WithDictionary]) -> None:
    reveal_type(cls.__dict__)  # revealed: dict[str, Any]
```

## Slot assignments preserve inferred types

Assignments to slotted attributes still determine their inferred public types.

```py
class Slotted:
    __slots__ = ("value",)

    def __init__(self, value: int) -> None:
        self.value = value

reveal_type(Slotted(1).value)  # revealed: int
```

## Slot assignments preserve flow-sensitive narrowing

Writing a value directly into an annotated slot narrows later reads without changing which values
the slot accepts.

```py
class Slotted:
    __slots__ = ("value",)

    def __init__(self) -> None:
        self.value: int | None = None

    def assign(self) -> int:
        self.value = 1
        reveal_type(self.value)  # revealed: Literal[1]
        return self.value

    def initialize(self) -> int:
        if self.value is None:
            self.value = 1
        reveal_type(self.value)  # revealed: int
        return self.value

    def reject(self) -> None:
        self.value = "wrong"  # error: [invalid-assignment]

class ClassAnnotated:
    __slots__ = ("value",)
    value: int | None

    def assign(self) -> int:
        self.value = 1
        reveal_type(self.value)  # revealed: Literal[1]
        return self.value
```

An arbitrary data descriptor can transform an assigned value, so it does not receive the same
storage-specific narrowing.

```py
class TransformingDescriptor:
    def __get__(self, instance: object, owner: type | None = None) -> int | None: ...
    def __set__(self, instance: object, value: int) -> None: ...

class DescriptorOwner:
    __slots__ = ()
    value = TransformingDescriptor()

def inspect_descriptor(owner: DescriptorOwner) -> None:
    owner.value = 1
    reveal_type(owner.value)  # revealed: int | None
```

## Annotated slots enforce their declared types

An annotation on a slot controls both attribute reads and assignments.

```py
class Slotted:
    __slots__ = ("value",)
    value: int

reveal_type(Slotted().value)  # revealed: int
Slotted().value = 1
Slotted().value = "wrong"  # error: [invalid-assignment]
```

## Slotted attributes declared in stub files

A stub annotation describes a runtime slot without creating a conflicting class variable, whether
the annotation has an ellipsis placeholder or no value at all.

```pyi
from types import MemberDescriptorType

class Slotted:
    __slots__ = ("value", "other")
    value: int
    other: str = ...

reveal_type(Slotted.value)  # revealed: MemberDescriptorType
reveal_type(Slotted.other)  # revealed: MemberDescriptorType

instance = Slotted()
reveal_type(instance.value)  # revealed: int
reveal_type(instance.other)  # revealed: str
instance.value = 1
instance.value = "wrong"  # error: [invalid-assignment]
instance.other = "valid"
instance.other = 1  # error: [invalid-assignment]
```

## Standard-library slot declarations

Standard-library stubs declare writable slotted attributes using ordinary annotations.

```py
from tarfile import TarInfo
from zipfile import ZipInfo

tar_info = TarInfo("example")
reveal_type(tar_info.size)  # revealed: int
tar_info.size = 1
tar_info.name = "updated"
tar_info.size = "wrong"  # error: [invalid-assignment]

zip_info = ZipInfo("example")
reveal_type(zip_info.external_attr)  # revealed: int
zip_info.external_attr = 1
zip_info.filename = "updated"
zip_info.external_attr = "wrong"  # error: [invalid-assignment]
```

## Slot descriptors preserve generic specialization

A generic slot has the value type provided by the receiver's specialization.

```py
from typing import Generic, TypeVar

T = TypeVar("T")

class Box(Generic[T]):
    __slots__ = ("value",)
    value: T

    def __init__(self, value: T) -> None:
        self.value = value

reveal_type(Box(1).value)  # revealed: int
Box(1).value = "wrong"  # error: [invalid-assignment]
```

## Slot attributes can be deleted

Slot descriptors support deleting their stored values as well as reading and writing them.

```py
class Slotted:
    __slots__ = ("value",)

instance = Slotted()
instance.value = 1
del instance.value
```

## Attributes initialized in `__new__`

Slots declare attributes initialized on the instance returned by `__new__`, including attributes
that are later modified by augmented assignments.

```py
class Counter:
    __slots__ = ("value",)

    def __new__(cls):
        instance = super().__new__(cls)
        instance.value = 0
        return instance

    def increment(self) -> None:
        self.value += 1

    def current(self):
        return self.value

reveal_type(Counter().value)  # revealed: Unknown
```

## Supported slot declaration forms

A single string names one slot. Literal tuples, lists, sets, and dictionaries provide their slot
names as elements or dictionary keys.

```py
class StringSlots:
    __slots__ = "value"

class TupleSlots:
    __slots__ = ("first", "second")

class ListSlots:
    __slots__ = ["value"]

class SetSlots:
    __slots__ = {"value"}

class DictionarySlots:
    __slots__ = {"value": "Documentation for the slot."}

reveal_type(StringSlots().value)  # revealed: Unknown
reveal_type(TupleSlots().first)  # revealed: Unknown
reveal_type(TupleSlots().second)  # revealed: Unknown
reveal_type(ListSlots().value)  # revealed: Unknown
reveal_type(SetSlots().value)  # revealed: Unknown
reveal_type(DictionarySlots().value)  # revealed: Unknown
```

## Annotated and indirect slot declarations

An annotation on `__slots__` does not hide its runtime value. A statically known tuple can also be
supplied through another variable.

```py
class AnnotatedSlots:
    __slots__: tuple[str, ...] = ("value",)

slot_names = ("value",)

class IndirectSlots:
    __slots__ = slot_names

reveal_type(AnnotatedSlots().value)  # revealed: Unknown
reveal_type(IndirectSlots().value)  # revealed: Unknown
```

## Mutated slot declarations

A mutable `__slots__` value can change before the class is created. If the class body uses that
value after assigning it, its contents cannot be assumed to remain unchanged.

```py
class MutatedSlots:
    __slots__ = ["value"]
    __slots__.append("extra")

    def __init__(self) -> None:
        self.extra = 1

reveal_type(MutatedSlots().extra)  # revealed: int
```

## Dynamic slot declarations

When the slot names cannot be determined statically, attribute writes remain permissive.

```py
def choose_slots() -> tuple[str, ...]:
    return ("value",)

class DynamicSlots:
    __slots__ = choose_slots()

    def __init__(self) -> None:
        self.value = 1
        self.extra = 2

reveal_type(DynamicSlots().extra)  # revealed: int
```

## Inherited slots

A slotted subclass can use slots declared by any of its base classes.

```py
class Base:
    __slots__ = ("base_value",)

class Child(Base):
    __slots__ = ("child_value",)

    def __init__(self) -> None:
        self.base_value = 1
        self.child_value = 2

reveal_type(Child().base_value)  # revealed: int
reveal_type(Child().child_value)  # revealed: int
```

## Inherited unknown declarations

Handling an unannotated slot must not change the precedence of an unrelated inherited instance
attribute whose explicitly declared type is unknown.

```py
class Base:
    def initialize(self) -> None:
        self.value: Missing = None  # error: [unresolved-reference]

class Child(Base):
    def initialize(self) -> None:
        self.value = 1

reveal_type(Child().value)  # revealed: Unknown
```

## Extra instance attributes require an instance dictionary

An instance without a dictionary cannot create attributes outside its declared slots.

```py
class Slotted:
    __slots__ = ("value",)
    shared = 1

    def __init__(self) -> None:
        self.value = 1
        self.extra = 2  # error: [unresolved-attribute]

Slotted().other = 3  # error: [unresolved-attribute]
Slotted().shared = 3  # error: [unresolved-attribute]
```

An explicit `__dict__` slot restores support for additional instance attributes.

```py
class WithDictionary:
    __slots__ = ("value", "__dict__")

    def __init__(self) -> None:
        self.extra = 1

reveal_type(WithDictionary().extra)  # revealed: int
```

An ordinary base class can also supply an inherited instance dictionary.

```py
class OrdinaryBase:
    pass

class InheritedDictionary(OrdinaryBase):
    __slots__ = ("value",)

    def __init__(self) -> None:
        self.extra = 1

reveal_type(InheritedDictionary().extra)  # revealed: int
```

A subclass without its own `__slots__` regains an instance dictionary.

```py
class SlottedBase:
    __slots__ = ("value",)

class OrdinaryChild(SlottedBase):
    def __init__(self) -> None:
        self.extra = 1

reveal_type(OrdinaryChild().extra)  # revealed: int
```

## Synthesized dataclass slots

Dataclass-generated slots have the same instance layout as slots written directly in the class body.
Subclasses inherit that layout unless they introduce an instance dictionary.

```py
from dataclasses import dataclass

@dataclass(slots=True)
class SlottedDataclass:
    value: int

SlottedDataclass(1).__dict__  # error: [unresolved-attribute]

class SlottedChild(SlottedDataclass):
    __slots__ = ("other",)

    def initialize(self) -> None:
        self.extra = 1  # error: [unresolved-attribute]

SlottedChild(1).__dict__  # error: [unresolved-attribute]
```

A `KW_ONLY` sentinel changes constructor parameter ordering but does not need instance storage,
regardless of the sentinel's name.

```py
from dataclasses import KW_ONLY

@dataclass(slots=True)
class KeywordOnlySlots:
    required: int
    marker: KW_ONLY
    keyword_only: int

reveal_type(KeywordOnlySlots.__slots__)  # revealed: tuple[Literal["required"], Literal["keyword_only"]]

class OrdinaryWithMarker:
    __slots__ = ("required",)
    marker: KW_ONLY
```

## Synthesized dataclass slots exclude inherited storage

A slotted dataclass creates descriptors only for fields that do not already have an inherited slot.

```py
from dataclasses import dataclass

@dataclass(slots=True)
class Parent:
    value: int

@dataclass(slots=True)
class Child(Parent):
    other: int

reveal_type(Child.__slots__)  # revealed: tuple[Literal["other"]]

@dataclass(slots=True)
class Redefined(Parent):
    value: int
    other: int

reveal_type(Redefined.__slots__)  # revealed: tuple[Literal["other"]]
```

An inherited field still needs a new slot when its original class stored the field in an instance
dictionary.

```py
@dataclass
class UnslottedParent:
    value: int

@dataclass(slots=True)
class SlottedChild(UnslottedParent):
    other: int

reveal_type(SlottedChild.__slots__)  # revealed: tuple[Literal["value"], Literal["other"]]
```

An ordinary slotted base also supplies storage for any matching dataclass field.

```py
class SlottedBase:
    __slots__ = ("value",)

@dataclass(slots=True)
class SlottedChild(SlottedBase):
    value: int
    other: int

reveal_type(SlottedChild.__slots__)  # revealed: tuple[Literal["other"]]
```

## Synthesized dataclass slots on Python 3.10

Python 3.10 includes inherited fields in generated dataclass slots. For consistency across Python
versions, ty intentionally uses the Python 3.11-and-later behavior when targeting Python 3.10.

```toml
[environment]
python-version = "3.10"
```

```py
from dataclasses import dataclass

@dataclass(slots=True)
class Parent:
    value: int

@dataclass(slots=True)
class Child(Parent):
    other: int

reveal_type(Child.__slots__)  # revealed: tuple[Literal["other"]]
```

## Slots generated by dataclass transforms

A dataclass-like decorator can also generate slots. The resulting class has the same restricted
instance layout as an ordinary slotted dataclass.

```py
from typing import Callable, TypeVar
from typing_extensions import dataclass_transform

T = TypeVar("T", bound=type)

@dataclass_transform()
def model(*, slots: bool = False) -> Callable[[T], T]:
    raise NotImplementedError

@model(slots=True)
class SlottedModel:
    value: int

SlottedModel(1).__dict__  # error: [unresolved-attribute]
SlottedModel(1).other = 1  # error: [unresolved-attribute]
```

## Builtin bases without instance dictionaries

A slotted subclass of a builtin without an instance dictionary cannot create extra attributes.

```py
class SlottedString(str):
    __slots__ = ("value",)

    def initialize(self) -> None:
        self.extra = 1  # error: [unresolved-attribute]

SlottedString("value").__dict__  # error: [unresolved-attribute]
```

## Descriptors and custom attribute setters

Data descriptors can handle writes without an instance dictionary, and a custom `__setattr__` can
implement its own storage policy.

```py
class Descriptor:
    def __set__(self, instance: object, value: int) -> None: ...

class SlottedDescriptor:
    __slots__ = ()
    value = Descriptor()

SlottedDescriptor().value = 1
SlottedDescriptor().value = "wrong"  # error: [invalid-assignment]

class CustomSetter:
    __slots__ = ()
    shared = 1

    def __setattr__(self, name: str, value: int) -> None: ...

CustomSetter().shared = 1
```

A descriptor may be hidden behind a gradual class-body annotation. Its runtime setter must remain
available even though its declared type does not expose `__set__`.

```py
from typing import Any

class Descriptor:
    def __set__(self, instance: object, value: int) -> None: ...

class SlottedDescriptor:
    __slots__ = ()
    value: Any = Descriptor()

SlottedDescriptor().value = 1
```

## Instance dictionary and weak-reference slots

A slotted instance has neither `__dict__` nor `__weakref__` unless explicitly requested or
inherited.

```py
from types import GetSetDescriptorType

class Slotted:
    __slots__ = ("value",)

Slotted().__dict__  # error: [unresolved-attribute]
Slotted().__weakref__  # error: [unresolved-attribute]

class WithSpecialSlots:
    __slots__ = ("value", "__dict__", "__weakref__")

WithSpecialSlots().__dict__
WithSpecialSlots().__weakref__
reveal_type(WithSpecialSlots.__dict__)  # revealed: dict[str, Any]
reveal_type(WithSpecialSlots.__weakref__)  # revealed: GetSetDescriptorType

def accepts_get_set_descriptor(descriptor: GetSetDescriptorType) -> None: ...

accepts_get_set_descriptor(WithSpecialSlots.__weakref__)
WithSpecialSlots().__weakref__ = None  # error: [invalid-assignment]
del WithSpecialSlots().__weakref__  # error: [invalid-assignment]
```

A slotted class can still define a descriptor named `__dict__` without providing ordinary instance
dictionary storage.

```py
class VirtualDictionary:
    __slots__ = ()

    @property
    def __dict__(self) -> dict[str, int]:
        return {"virtual": 1}

reveal_type(VirtualDictionary().__dict__)  # revealed: dict[str, int]
```

## Weak-reference storage inherited from ordinary bases

Ordinary classes provide weak-reference storage at runtime, but their implicit `__weakref__`
attributes are not modeled. The same limitation applies to slotted subclasses.

```toml
[environment]
python-version = "3.11"
```

```py
class OrdinaryBase:
    pass

class SlottedChild(OrdinaryBase):
    __slots__ = ("value",)

OrdinaryBase().__weakref__  # error: [unresolved-attribute]
SlottedChild().__weakref__  # error: [unresolved-attribute]
```

Without modeling that inherited storage, a slotted dataclass also includes a requested
weak-reference slot even though the ordinary base already provides it at runtime.

```py
from dataclasses import dataclass

@dataclass(slots=True, weakref_slot=True)
class SlottedDataclass(OrdinaryBase):
    value: int

reveal_type(SlottedDataclass.__slots__)  # revealed: tuple[Literal["value"], Literal["__weakref__"]]
```

## Class-body annotations do not require instance storage

A bare annotation can describe an attribute provided by a subclass, a dynamically installed
descriptor, or a custom attribute accessor. The annotation itself does not require an instance slot.

```py
from typing import ClassVar, TYPE_CHECKING

class Slotted:
    __slots__ = ("value",)
    value: int
    shared: ClassVar[int] = 1
    missing: int

    if TYPE_CHECKING:
        dynamic: int

Slotted().missing = 1  # error: [unresolved-attribute]

class Mixin:
    __slots__ = ()
    provided_by_subclass: int

class Child(Mixin):
    __slots__ = ("provided_by_subclass",)
```

## Class variables cannot conflict with slots

A class variable with the same name as a slot prevents the class from being created at runtime.

```py
class Conflicting:
    __slots__ = ("value",)
    value = 1  # error: [invalid-assignment]
```

A method with the same name also occupies the final class namespace and conflicts with the slot.

```py
class ConflictingMethod:
    __slots__ = ("value",)

    def value(self) -> None:  # error: [invalid-assignment]
        pass
```

A temporary class variable that is deleted before the class is created does not conflict.

```py
class DeletedDefault:
    __slots__ = ("value",)
    value = 1
    del value
```

## Variable-length builtins cannot add slots

Subclasses of variable-length builtin types cannot declare additional instance slots.

```py
class SlottedInteger(int):  # error: [instance-layout-conflict]
    __slots__ = ("value",)

class SlottedBytes(bytes):  # error: [instance-layout-conflict]
    __slots__ = ("value",)

class SlottedTuple(tuple[object, ...]):  # error: [instance-layout-conflict]
    __slots__ = ("value",)

class EmptyInteger(int):
    __slots__ = ()
```
