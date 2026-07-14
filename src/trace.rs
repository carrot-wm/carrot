// zero-cost tracing: `--features trace` compiles the probes in, else the
// macro expands to nothing.

#[cfg(feature = "trace")]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        eprintln!("[{}:{}] {}", file!(), line!(), format_args!($($arg)*))
    };
}

// the disabled arm still names its args so bindings that only feed a
// probe don't warn; the dead branch folds away
#[cfg(not(feature = "trace"))]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        if false {
            let _ = format_args!($($arg)*);
        }
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn expands_in_both_modes() {
        crate::trace!("probe {}", 1);
    }
}
