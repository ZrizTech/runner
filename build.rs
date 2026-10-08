//! Emits `ZRIZ_BUILD`: `<version>+<sha>` when `ZRIZ_BUILD_SHA` is 12 lowercase
//! hex chars, else `<version>+dev`.

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let sha = std::env::var("ZRIZ_BUILD_SHA").unwrap_or_default();
    let ok = sha.len() == 12 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let tail = if ok { sha.as_str() } else { "dev" };
    println!("cargo:rustc-env=ZRIZ_BUILD={version}+{tail}");
    println!("cargo:rerun-if-env-changed=ZRIZ_BUILD_SHA");
}
