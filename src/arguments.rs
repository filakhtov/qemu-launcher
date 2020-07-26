use std::io::{Error, ErrorKind};

pub struct Arguments {
    debug: bool,
    show_usage: bool,
    machine_name: String,
    verbose: bool,
}

impl Arguments {
    pub fn is_debug_enabled(&self) -> bool {
        self.debug
    }

    pub fn is_verbose_mode(&self) -> bool {
        self.verbose || self.is_debug_enabled()
    }

    pub fn should_show_usage(&self) -> bool {
        self.show_usage
    }

    pub fn get_machine_name(&self) -> &str {
        &self.machine_name
    }

    pub fn new(arguments: &Vec<String>) -> Result<Self, Error> {
        let mut verbose = false;
        let mut debug = false;
        let mut machine_name = String::new();
        let mut usage = false;
        let mut has_machine_name = false;

        for argument in arguments {
            match argument.as_str() {
                "-v" => {
                    verbose = true;
                }
                "-d" => {
                    debug = true;
                }
                "-h" => {
                    usage = true;
                }
                _ => {
                    if has_machine_name {
                        return Err(Error::new(ErrorKind::Other, "Too many parameters."));
                    }

                    machine_name = argument.to_owned();
                    has_machine_name = true;
                }
            }
        }

        if usage {
            return Ok(Self {
                verbose: false,
                debug: false,
                show_usage: true,
                machine_name: String::new(),
            });
        }

        if !has_machine_name {
            return Err(Error::new(
                ErrorKind::Other,
                "Missing virtual machine name.",
            ));
        }

        validate_machine_name(&machine_name)?;

        Ok(Self {
            verbose: verbose,
            debug: debug,
            show_usage: usage,
            machine_name: machine_name,
        })
    }
}

fn validate_machine_name(machine_name: &str) -> Result<(), Error> {
    if machine_name.contains("\0") || machine_name.contains("/") {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Machine name contains invalid characters.",
        ));
    }

    Ok({})
}

#[cfg(test)]
mod test {
    use super::Arguments;
    use std::io::{Error, ErrorKind};

    fn assert_result_is_error(
        result: Result<Arguments, Error>,
        expected_kind: ErrorKind,
        expected_message: &str,
    ) {
        match result {
            Ok(_) => panic!("Parser did not return an error"),
            Err(e) => {
                assert_eq!(expected_kind, e.kind());
                assert_eq!(expected_message, format!("{}", e));
            }
        }
    }

    #[test]
    fn arguments_accepts_machine_name() {
        let result = Arguments::new(&vec![String::from("my-vm")]);

        assert!(result.is_ok(), "Failed to parse arguments");

        let arguments = result.unwrap();

        assert!(
            !arguments.is_verbose_mode(),
            "Verbose mode is not enabled with `-v` flag"
        );
        assert!(
            !arguments.is_debug_enabled(),
            "Debug mode is enabled without `-d` flag"
        );
        assert!(
            !arguments.should_show_usage(),
            "Show usage is enabled without `-h` flag"
        );
        assert_eq!("my-vm", arguments.get_machine_name());
    }

    #[test]
    fn arguments_accepts_verbose_flag() {
        let result = Arguments::new(&vec![String::from("-v"), String::from("verbose-vm")]);

        assert!(result.is_ok(), "Failed to parse arguments");

        let arguments = result.unwrap();

        assert!(
            arguments.is_verbose_mode(),
            "Verbose mode is not enabled with `-v` flag"
        );
    }

    #[test]
    fn arguments_accepts_debug_flag() {
        let result = Arguments::new(&vec![String::from("-d"), String::from("debug-vm")]);

        assert!(result.is_ok(), "Failed to parse arguments");

        let arguments = result.unwrap();

        assert!(
            arguments.is_verbose_mode(),
            "Debug mode is not enabled with `-d` flag"
        );
        assert!(
            arguments.is_debug_enabled(),
            "Verbose mode is not enabled with `-d` flag"
        );
    }

    #[test]
    fn arguments_ignores_duplicate_flags() {
        let result = Arguments::new(&vec![
            String::from("-v"),
            String::from("debug-vm"),
            String::from("-v"),
            String::from("-d"),
            String::from("-d"),
        ]);

        assert!(result.is_ok(), "Failed to parse arguments");

        let arguments = result.unwrap();

        assert!(
            arguments.is_verbose_mode(),
            "Debug mode is not enabled with `-d` flag"
        );
        assert!(
            arguments.is_debug_enabled(),
            "Verbose mode is not enabled with `-d` flag"
        );
    }

    #[test]
    fn arguments_reports_missing_machine_name() {
        let result = Arguments::new(&vec![]);

        assert_result_is_error(result, ErrorKind::Other, "Missing virtual machine name.");
    }

    #[test]
    fn arguments_discards_all_arguments_if_help_is_requested() {
        let result = Arguments::new(&vec![
            String::from("-v"),
            String::from("-d"),
            String::from("-h"),
            String::from("debug-vm"),
        ]);

        assert!(result.is_ok(), "Failed to parse arguments");

        let arguments = result.unwrap();

        assert!(
            !arguments.is_verbose_mode(),
            "Verbose mode is enabled with `-h` flag"
        );
        assert!(
            !arguments.is_debug_enabled(),
            "Debug mode is enabled with `-h` flag"
        );
        assert!(
            arguments.should_show_usage(),
            "Show usage is disabled with `-h` flag"
        );
        assert!(arguments.get_machine_name().is_empty());
    }

    #[test]
    fn arguments_reports_too_many_arguments() {
        let result = Arguments::new(&vec![String::from("my-vm"), String::from("your-vm")]);

        assert_result_is_error(result, ErrorKind::Other, "Too many parameters.");
    }

    #[test]
    fn arguments_reports_error_if_machine_name_contains_wrong_characters() {
        let result = Arguments::new(&vec![String::from("my-vm\0")]);

        assert_result_is_error(
            result,
            ErrorKind::InvalidInput,
            "Machine name contains invalid characters.",
        );
    }
}
