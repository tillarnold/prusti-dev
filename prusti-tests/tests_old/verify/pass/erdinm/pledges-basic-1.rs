use prusti_contracts::*;

#[ensures(assert_on_expiry(*result == 42))]
fn must_answer(location: &mut i32) -> &mut i32 {
    location
}

fn test() {
    let mut x = 0;
    let y = must_answer(&mut x);
    *y = 42;
}

fn main() {}
