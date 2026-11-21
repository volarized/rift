from shapes import Circle, Square, make, registry

DEFAULT = make()


def total() -> float:
    shape = make()
    return shape.area()


def each() -> float:
    result = 0.0
    for shape in [Square(), Square()]:
        result += shape.area()
    return result


def lookup() -> float:
    return registry()["circle"].area()


def chained() -> float:
    shapes = [Circle()]
    return shapes.pop().area()


def label() -> str:
    return str(len("x")).upper()
