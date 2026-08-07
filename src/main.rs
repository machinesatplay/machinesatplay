#[cfg(feature = "engine-core")]
mod runtime;

#[cfg(all(target_family = "wasm", feature = "client"))]
fn main() {}

#[cfg(all(target_family = "wasm", feature = "client"))]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn start_game(
    certificate_digest: &str,
    server_host: &str,
    server_port: u16,
) -> Result<(), wasm_bindgen::JsValue> {
    console_error_panic_hook::set_once();
    runtime::start_game(certificate_digest, server_host, server_port)
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    runtime::run_native();
}
