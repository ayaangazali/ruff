# Errors

s = set()

for x in [1, 2, 3]:
    s.add(x)

for x in {1, 2, 3}:
    s.add(x)

for x in (1, 2, 3):
    s.add(x)

for x in (1, 2, 3):
    s.discard(x)

for x in (1, 2, 3):
    s.add(x + 1)

for x, y in ((1, 2), (3, 4)):
    s.add((x, y))

num = 123

for x in (1, 2, 3):
    s.add(num)

for x in (1, 2, 3):
    s.add((num, x))

for x in (1, 2, 3):
    s.add(x + num)

# https://github.com/astral-sh/ruff/issues/15936
for x in 1, 2, 3:
    s.add(x)

for x in 1, 2, 3:
    s.add(f"{x}")

for x in (
    1,  # Comment
    2, 3
):
    s.add(f"{x}")


# False negative

class C:
    s: set[int]


c = C()
for x in (1, 2, 3):
    c.s.add(x)


# Ok

s.update(x for x in (1, 2, 3))

for x in (1, 2, 3):
    s.add(x)
else:
    pass


async def f(y):
    async for x in y:
        s.add(x)


def g():
    for x in (set(),):
        x.add(x)


# Test cases for lambda and ternary expressions - https://github.com/astral-sh/ruff/issues/18590

s = set()

for x in lambda: 0:
    s.discard(-x)

for x in (1,) if True else (2,):
    s.add(-x)

# don't add extra parens
for x in (lambda: 0):
    s.discard(-x)

for x in ((1,) if True else (2,)):
    s.add(-x)

# don't add parens directly in function call
for x in lambda: 0:
    s.discard(x)

for x in (1,) if True else (2,):
    s.add(x)

# https://github.com/astral-sh/ruff/issues/21098
for x in ("abc", "def"):
    s.add(c for c in x)

# don't add extra parens for already parenthesized generators
for x in ("abc", "def"):
    s.add((c for c in x))


# https://github.com/astral-sh/ruff/issues/18575
# OK: dropping the loop would leave the loop variable unbound
def target_used_after_loop():
    t = set()
    for x in (2, 3):
        t.add(x)
    print(x)


# OK: same, for one name of a tuple target
def tuple_target_used_after_loop():
    t = set()
    for a, b in [(1, 2)]:
        t.add(a)
    print(b)


# still flagged: the loop variable is not used afterwards
def target_not_used_after_loop():
    t = set()
    for x in (2, 3):
        t.add(x)
