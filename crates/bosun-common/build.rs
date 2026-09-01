fn main() {
    println!(
        "cargo:rustc-env=BOSUN_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
