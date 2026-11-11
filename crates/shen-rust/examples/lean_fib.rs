fn main() {
    match shen_rust::interp::lean_fib::fib10() {
        Ok(55) => println!("fib 10 = 55"),
        Ok(n) => {
            eprintln!("fib 10 = {n}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
