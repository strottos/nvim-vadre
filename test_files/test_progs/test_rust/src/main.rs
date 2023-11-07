use std::{io, thread, time::Duration};

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

    let thread_1 = thread::spawn(|| {
        for i in 0..10 {
            c(i)?;

            thread::sleep(Duration::from_secs(1));
        }

        Ok::<(), io::Error>(())
    });

    let thread_2 = thread::spawn(|| {
        for i in 50..60 {
            c(i)?;

            thread::sleep(Duration::from_secs(1));
        }

        Ok::<(), io::Error>(())
    });

    assert_eq!(thread_1.join().is_ok(), true);
    assert_eq!(thread_2.join().is_ok(), true);

    Ok(())
}
