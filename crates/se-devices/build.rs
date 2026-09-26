//! Generate V4L2 struct/constant bindings from the system `linux/videodev2.h`, so struct
//! layouts always match the running kernel's UAPI headers.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=/usr/include/linux/videodev2.h");
    let bindings = bindgen::Builder::default()
        .header_contents("se_v4l2.h", "#include <linux/videodev2.h>\n")
        .allowlist_type("v4l2_.*")
        .allowlist_var("V4L2_.*")
        .derive_default(true)
        .derive_debug(false)
        .layout_tests(false)
        .generate_comments(false)
        .prepend_enum_name(false)
        .generate()
        .expect("generate V4L2 bindings from linux/videodev2.h");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    bindings.write_to_file(out.join("v4l2_sys.rs")).expect("write V4L2 bindings");
}
