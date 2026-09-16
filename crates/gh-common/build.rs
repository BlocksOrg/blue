// `version::blue_version` reads BLUE_BUILD_VERSION through `option_env!`, which
// cargo does not track by itself: without this line a local rebuild after
// changing the variable silently keeps the previously baked string.
fn main() {
    println!("cargo:rerun-if-env-changed=BLUE_BUILD_VERSION");
}
