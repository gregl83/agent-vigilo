use std::io::{self, Read};

#[no_mangle]
pub extern "C" fn evaluate() -> i32 {
    // Read input from stdin (WASI)
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();

    // Do work
    let price: f64 = input.trim().parse().unwrap_or(0.0);
    let adjusted = (price * 1.1) as i32;

    adjusted
}
