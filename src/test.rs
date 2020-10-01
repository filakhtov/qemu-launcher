#[macro_export]
macro_rules! assert_error {
    ($kind:expr, $msg:expr, $result:expr) => {{
        match $result {
            Ok(_) => panic!("Expected an error, got successful result."),
            Err(e) => {
                assert_eq!($kind, e.kind());
                assert_eq!($msg, format!("{}", e));
            }
        }
    }};
}
