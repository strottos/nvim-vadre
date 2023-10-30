use std::io;

fn a() -> io::Result<()> {
    b()?;
    c(0)?;

    Ok(())
}

fn b() -> io::Result<()> {
    c(32)?;

    Ok(())
}

fn c(num: i32) -> io::Result<()> {
    if num != 0 {
        println!("Num: {}", num);
    }

    Ok(())
}

fn main() -> io::Result<()> {
    a()?;

    Ok(())
}
