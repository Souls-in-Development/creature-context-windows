//! Declare the Windows Phi Silica bridge seam.
//!
//! A future Windows App SDK project is responsible for compiling and linking
//! the native bridge. Cargo alone exercises the honest unavailable fallback.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(phi_silica_bridge)");
}
