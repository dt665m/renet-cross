#[cfg(not(target_arch = "wasm32"))]
mod client_native;
#[cfg(target_arch = "wasm32")]
mod client_web;
mod common;

fn main() {
    common::run();
}
