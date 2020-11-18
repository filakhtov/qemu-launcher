use std::path::Path;

const PROGRAM_NAME: &str = "qemu-launcher";

pub struct UsageArgs {
    program_name: String,
}

impl UsageArgs {
    pub fn get_program_name(&self) -> &String {
        &self.program_name
    }
}

pub struct ErrorArgs {
    program_name: String,
    error: &'static str,
}

impl ErrorArgs {
    pub fn get_program_name(&self) -> &String {
        &self.program_name
    }

    pub fn get_error(&self) -> &'static str {
        self.error
    }
}

pub struct ValidArgs {
    program_name: String,
    debug: bool,
    machine_name: String,
    verbose: bool,
}

impl ValidArgs {
    pub fn is_debug_enabled(&self) -> bool {
        self.debug
    }

    pub fn is_verbose_mode(&self) -> bool {
        self.verbose || self.is_debug_enabled()
    }

    pub fn get_machine_name(&self) -> &str {
        &self.machine_name
    }

    pub fn get_program_name(&self) -> &str {
        &self.program_name
    }
}

pub enum Arguments {
    Empty,
    Invalid(ErrorArgs),
    Valid(ValidArgs),
    Usage(UsageArgs),
}

impl Arguments {
    pub fn new(arguments: &Vec<String>) -> Arguments {
        if arguments.len() < 1 {
            return Arguments::Empty;
        }

        let program_name = match Path::new(&arguments[0]).file_name() {
            Some(name) => match name.to_str() {
                Some(name) => name,
                None => PROGRAM_NAME,
            },
            None => PROGRAM_NAME,
        }
        .to_owned();

        let mut verbose = false;
        let mut debug = false;
        let mut machine_name = String::new();
        let mut has_machine_name = false;

        for argument in &arguments[1..] {
            match argument.as_str() {
                "-v" => {
                    verbose = true;
                }
                "-d" => {
                    debug = true;
                }
                "-h" => return Arguments::Usage(UsageArgs { program_name }),
                _ => {
                    if has_machine_name {
                        return Arguments::Invalid(ErrorArgs {
                            program_name,
                            error: "Too many parameters.",
                        });
                    }

                    machine_name = argument.to_owned();
                    has_machine_name = true;
                }
            }
        }

        if !has_machine_name {
            return Arguments::Invalid(ErrorArgs {
                program_name,
                error: "Missing the guest machine name",
            });
        }

        if !is_valid_machine_name(&machine_name) {
            return Arguments::Invalid(ErrorArgs {
                program_name,
                error: "The machine name contains invalid characters.",
            });
        }

        Arguments::Valid(ValidArgs {
            program_name,
            verbose,
            debug,
            machine_name,
        })
    }
}

fn is_valid_machine_name(machine_name: &str) -> bool {
    if machine_name.contains("\0") || machine_name.contains("/") {
        return false;
    }

    true
}

#[cfg(test)]
mod test {
    use super::Arguments;

    #[test]
    fn arguments_accepts_machine_name() {
        let arguments = match Arguments::new(&vec![String::from("launcher"), String::from("my-vm")])
        {
            Arguments::Valid(v) => v,
            _ => panic!("Expected arguments to be valid"),
        };

        assert!(
            !arguments.is_verbose_mode(),
            "Verbose mode is not enabled with `-v` flag"
        );
        assert!(
            !arguments.is_debug_enabled(),
            "Debug mode is enabled without `-d` flag"
        );
        assert_eq!("my-vm", arguments.get_machine_name());
        assert_eq!("launcher", arguments.get_program_name());
    }

    #[test]
    fn arguments_accepts_verbose_flag() {
        let arguments = match Arguments::new(&vec![
            String::from("launcher"),
            String::from("-v"),
            String::from("verbose-vm"),
        ]) {
            Arguments::Valid(v) => v,
            _ => panic!("Expected arguments to be valid"),
        };

        assert!(
            arguments.is_verbose_mode(),
            "Verbose mode is not enabled with `-v` flag"
        );
    }

    #[test]
    fn arguments_accepts_debug_flag() {
        let arguments = match Arguments::new(&vec![
            String::from("launcher"),
            String::from("-d"),
            String::from("debug-vm"),
        ]) {
            Arguments::Valid(v) => v,
            _ => panic!("Expected arguments to be valid"),
        };

        assert!(
            arguments.is_verbose_mode(),
            "Verbose mode is not enabled with `-d` flag"
        );
        assert!(
            arguments.is_debug_enabled(),
            "Debug mode is not enabled with `-d` flag"
        );
    }

    #[test]
    fn arguments_ignores_duplicate_flags() {
        let arguments = match Arguments::new(&vec![
            String::from("launcher"),
            String::from("-v"),
            String::from("debug-vm"),
            String::from("-v"),
            String::from("-d"),
            String::from("-d"),
        ]) {
            Arguments::Valid(v) => v,
            _ => panic!("Expected arguments to be valid"),
        };

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
    fn arguments_reports_empty_arguments() {
        match Arguments::new(&vec![]) {
            Arguments::Empty => {}
            _ => panic!("Expected arguments to be an empty instance"),
        }
    }

    #[test]
    fn arguments_reports_missing_machine_name() {
        let result = match Arguments::new(&vec![String::from("qemu")]) {
            Arguments::Invalid(e) => e,
            _ => panic!("Expected arguments to be invalid"),
        };

        assert_eq!("Missing the guest machine name", result.get_error());
        assert_eq!("qemu", result.get_program_name());
    }

    #[test]
    fn arguments_discards_all_arguments_if_help_is_requested() {
        let arguments = match Arguments::new(&vec![
            String::from("launcher"),
            String::from("-v"),
            String::from("-d"),
            String::from("-h"),
            String::from("debug-vm"),
        ]) {
            Arguments::Usage(u) => u,
            _ => panic!("Expected arguments to be a usage instance"),
        };

        assert_eq!("launcher", arguments.get_program_name());
    }

    #[test]
    fn arguments_reports_too_many_arguments() {
        let result = match Arguments::new(&vec![
            String::from("launcher"),
            String::from("my-vm"),
            String::from("your-vm"),
        ]) {
            Arguments::Invalid(e) => e,
            _ => panic!("Expected arguments to be invalid"),
        };

        assert_eq!("Too many parameters.", result.get_error());
        assert_eq!("launcher", result.get_program_name());
    }

    #[test]
    fn arguments_reports_error_if_machine_name_contains_wrong_characters() {
        let result = match Arguments::new(&vec![String::from("launcher"), String::from("my-vm\0")])
        {
            Arguments::Invalid(e) => e,
            _ => panic!("Expected arguments to be invalid"),
        };

        assert_eq!(
            "The machine name contains invalid characters.",
            result.get_error(),
        );
        assert_eq!("launcher", result.get_program_name());
    }

    #[test]
    fn arguments_uses_default_program_name_if_missing_from_input() {
        let arguments = match Arguments::new(&vec![String::from(""), String::from("myvm")]) {
            Arguments::Valid(v) => v,
            _ => panic!("Expected arguments to be valid"),
        };

        assert_eq!(super::PROGRAM_NAME, arguments.get_program_name());
        assert_eq!("myvm", arguments.get_machine_name());
    }
}
