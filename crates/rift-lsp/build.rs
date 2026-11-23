//! Enables lifecycle evidence only in the isolated test packet.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(rift_test_lifecycle_trace)");
    println!("cargo::rerun-if-env-changed=RIFT_TEST_LIFECYCLE_TRACE");
    if std::env::var("RIFT_TEST_LIFECYCLE_TRACE").as_deref() == Ok("1") {
        println!("cargo::rustc-cfg=rift_test_lifecycle_trace");
    }
}
