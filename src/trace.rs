// zero-cost tracing: `--features trace` compiles the probes in, else the
// macro expands to nothing.

#[cfg(feature = "trace")]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        eprintln!("[{}:{}] {}", file!(), line!(), format_args!($($arg)*))
    };
}

#[cfg(not(feature = "trace"))]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{}};
}

#[cfg(test)]
mod tests {
    #[test]
    fn expands_in_both_modes() {
        crate::trace!("probe {}", 1);
    }
}
