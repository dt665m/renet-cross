mod common;
#[cfg(not(target_arch = "wasm32"))]
mod client_native;
#[cfg(target_arch = "wasm32")]
mod client_web;

fn main() {
    common::run();
}
