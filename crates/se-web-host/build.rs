//! The CEF runtime (libcef.so and its resources) is installed next to the binary, so the
//! binary finds libcef through `$ORIGIN` without `LD_LIBRARY_PATH`.

fn main() {
    println!("cargo::rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
}
