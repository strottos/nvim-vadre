use std::io;

use anyhow::Result;

fn a() -> String {
    b()
}

fn b() -> String {
    "my friend".to_string()
}

fn main() -> Result<()> {
    println!("Who are you {}", a());
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer)?;
    println!("Hello {}, {}", buffer.trim_end(), b());

    Ok(())
}
