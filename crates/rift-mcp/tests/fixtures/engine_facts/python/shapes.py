class Shape:
    def area(self) -> float:
        return 0.0


class Square(Shape):
    def area(self) -> float:
        return 1.0


class Circle(Shape):
    def area(self) -> float:
        return 3.0


def make():
    return Square()


def registry():
    return {"circle": Circle()}
