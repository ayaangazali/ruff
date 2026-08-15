# Errors

nums = {1, 2, 3}
for num in nums:
    nums.add(num + 1)

animals = {"dog", "cat", "cow"}
for animal in animals:
    animals.pop("cow")

fruits = {"apple", "orange", "grape"}
for fruit in fruits:
    fruits.clear()

planets = {"mercury", "venus", "earth"}
for planet in planets:
    planets.discard("mercury")

colors = {"red", "green", "blue"}
for color in colors:
    colors.remove("red")

odds = {1, 3, 5}
for num in odds:
    if num > 1:
        odds.add(num + 1)

# OK

nums = {1, 2, 3}
for num in nums.copy():
    nums.add(nums + 3)

animals = {"dog", "cat", "cow"}
for animal in animals:
    print(animals - {animal})

fruits = {"apple", "orange", "grape"}
temp_fruits = set()
for fruit in fruits:
    temp_fruits.add(fruit)
    temp_fruits.remove(fruit)
    temp_fruits.clear(fruit)

colors = {"red", "green", "blue"}


def add_colors():
    colors = {"cyan", "magenta", "yellow"}
    for color in colors:

        def add_color():
            global colors
            colors.add(color)

        add_color()


add_colors()
print(colors)


# https://github.com/astral-sh/ruff/issues/18818
def rebound_set():
    nums = {1, 2, 3}
    nums = {1, 2, 3}
    for num in nums:  # PLE4703
        nums.add(num + 5)


# OK: one of the visible bindings is not a set
def not_always_a_set(cond):
    if cond:
        a = {1, 2}
    else:
        a = {}
    for x in a:
        a.add(x)


# OK: `set` here is the builtin class, not a set
for item in set:
    set.add(item)
