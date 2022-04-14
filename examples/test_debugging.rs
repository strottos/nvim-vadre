fn a() -> u32 {
    b()
}

fn b() -> u32 {
    64
}

fn main() {
    println!("HELLO: {}", a());
}
