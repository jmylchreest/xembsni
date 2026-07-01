//! Thin entry point. All logic lives in the library (`xembsni::run`) so it can
//! be unit-tested without launching the binary.

fn main() -> anyhow::Result<()> {
    xembsni::run()
}
