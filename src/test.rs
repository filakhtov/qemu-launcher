#[macro_export]
#[cfg(test)]
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

#[macro_export]
#[cfg(test)]
macro_rules! vec_deq {
    [] => {{
        VecDeque::new()
    }};
    [ $( $item:expr ),+ $(,)? ] => {{
        let mut v = VecDeque::new();
        $( v.push_back($item); )*
        v
    }};
}

#[macro_export]
#[cfg(test)]
macro_rules! verify_expectations {
    ($($call:path => $var:ident::$field:ident),* $(,)?) => {{
        $(
            let len = $var.with(|expectations| expectations.borrow().$field.len());
            if len > 0 {
                panic!("{} more {} call(s) expected.", len, stringify!($call));
            }
        )*
    }};
}

#[macro_export]
#[cfg(test)]
macro_rules! create_format {
    ($e:expr) => { "{:?}" };
    ($e:expr, $($es:expr),+) => { concat!("{:?}, ", crate::create_format!($($es),*)) };
}

#[macro_export]
#[cfg(test)]
macro_rules! verify_expectation {
    ($var:ident::$field:ident => $call:path $({ _ })?) => {
        $var.with(|expectations| {
            let (_, result) = match expectations.borrow_mut().$field.pop_front() {
                Some(call) => call,
                None => panic!("Unexpected call to {}()", stringify!($call)),
            };

            result
        });
    };
    ($var:ident::$field:ident => $call:path { $arg:expr } ) => {
        $var.with(|expectations| {
            let (arg, result) = match expectations.borrow_mut().$field.pop_front() {
                Some(call) => call,
                None => panic!("Unexpected call to {}({:?})", stringify!($call), $arg),
            };

            if $arg != arg {
                panic!(
                    "Unexpected call to {}({:?}), expected: {}({:?})",
                    stringify!($call),
                    $arg,
                    stringify!($call),
                    arg
                )
            }

            result
        });
    };
    ($var:ident::$field:ident => $call:path { $( $args:expr ),+ $(,)? } ) => {
        $var.with(|expectations| {
            let (args, result) = match expectations.borrow_mut().$field.pop_front() {
                Some(call) => call,
                None => panic!(
                    concat!("Unexpected call to {}(", crate::create_format!($($args),*), ")"),
                    stringify!($call)$(, $args)*
                ),
            };

            if ($( $args, )*) != args {
                panic!(
                    concat!("Unexpected call to {}(", crate::create_format!($($args),*), "), expected: {}{:?}"),
                    stringify!($call)$(, $args)*, stringify!($call), args
                );
            }

            result
        });
    };
}

#[macro_export]
#[cfg(test)]
macro_rules! expect {
    ($var:ident::$field:ident: { _ => _ } $(,)?) => {{
        $var.with(|expectations| {
            expectations.borrow_mut().$field.push_back(((), ()));
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr => _ } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back(($arg, ())); )*
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr$(, $args:expr)+ => _ } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back((($arg$(, $args)*), ())); )*
        });
    }};
    ($var:ident::$field:ident: $( { _ => $result:expr } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back(((), $result)); )*
        });
    }};
    ($var:ident::$field:ident: $( { _ => $result:expr$(, $results:expr)+ } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back(((), ($result$(, $results)*))); )*
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr => $result:expr } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back(($arg, $result)); )*
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr$(, $args:expr)+ => $result:expr } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back((($arg$(, $args)*), $result)); )*
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr => $result:expr$(, $results:expr)+ } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back(($arg, ($result$(, $results)*))); )*
        });
    }};
    ($var:ident::$field:ident: $( { $arg:expr$(, $args:expr)+ => $result:expr$(, $results:expr)+ } ),+ $(,)?) => {{
        $var.with(|expectations| {
            $( expectations.borrow_mut().$field.push_back((($arg$(, $args)*), ($result$(, $results)*))); )*
        });
    }};
}
