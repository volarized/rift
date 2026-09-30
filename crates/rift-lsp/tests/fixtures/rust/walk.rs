use crate::hub::beacon;

pub struct Beacon {
    pub level: i32,
}

pub fn larger() -> i32 {
    let values = vec![1, 2];
    std::cmp::max(beacon(2), values.len() as i32)
}
