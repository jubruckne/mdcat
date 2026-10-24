fn main() {
    // Link arguments emitted by dependencies do not propagate to our executables/tests.
    // The Foundation Models bridge depends on the system Swift concurrency runtime.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
