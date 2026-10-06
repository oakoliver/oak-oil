fn main() {
    let n = fx_core::add(fx_gen::ANSWER, fx_shim::fx_shim());
    println!("{} {n}", fx_core::greet("oil"));
}
