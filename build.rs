// SPDX-License-Identifier: MIT
//! Build script. Only does work for the optional `differential` feature: it
//! links the C `libmimalloc` so the differential test can call it via FFI.
//! Set `MIMALLOC_C_LIB` to the directory containing `libmimalloc.so`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MIMALLOC_C_LIB");

    // `CARGO_FEATURE_DIFFERENTIAL` is set by Cargo when the feature is enabled.
    if std::env::var_os("CARGO_FEATURE_DIFFERENTIAL").is_none() {
        return;
    }
    match std::env::var("MIMALLOC_C_LIB") {
        Ok(dir) if !dir.is_empty() => {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=dylib=mimalloc");
            // Embed an rpath so the .so is found at run time without LD_LIBRARY_PATH.
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
            // Signal that the C library is actually linked, so the differential
            // test compiles its `extern "C" mi_*` block (which would otherwise
            // silently resolve to our own `capi` exports — a false oracle).
            println!("cargo:rustc-cfg=have_c_mimalloc");
        }
        _ => {
            println!(
                "cargo:warning=feature `differential` is on but MIMALLOC_C_LIB is unset; \
                 the differential test is skipped (set it to a directory containing libmimalloc.so)"
            );
        }
    }
}
