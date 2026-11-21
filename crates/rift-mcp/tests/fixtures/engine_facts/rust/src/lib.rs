pub trait Shape {
    fn area(&self) -> f64;
}

pub struct Square;

pub struct Circle;

impl Shape for Square {
    fn area(&self) -> f64 {
        1.0
    }
}

impl Shape for Circle {
    fn area(&self) -> f64 {
        3.0
    }
}

impl Default for Square {
    fn default() -> Self {
        Square
    }
}

pub fn boxed() -> Vec<Circle> {
    vec![Circle]
}

pub fn total() -> f64 {
    let shape = Square::default();
    shape.area()
}

pub fn inferred() -> f64 {
    let shape: Square = Default::default();
    shape.area()
}

pub fn each() -> f64 {
    boxed().iter().map(|shape| shape.area()).sum()
}

pub fn first() -> f64 {
    boxed().into_iter().next().map_or(0.0, |shape| shape.area())
}
