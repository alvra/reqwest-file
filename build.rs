fn main() {
    #[cfg(all(feature = "tokio_io", feature = "futures_io"))]
    compile_error!("feature \"tokio_io\" and feature \"futures_io\" cannot be enabled at the same time");

    #[cfg(not(any(feature = "tokio_io", feature = "futures_io")))]
    compile_error!("either feature \"tokio_io\" or feature \"futures_io\" needs to be enabled");
}
