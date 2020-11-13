#[cfg(not(test))]
use std::process::Command;
use std::{
    ffi::OsStr,
    io::{Error, ErrorKind, Result},
};
#[cfg(test)]
use test::std::process::Command;

pub struct Process {}

impl Process {
    pub fn oneshot<C: AsRef<OsStr>, I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
        command: C,
        arguments: I,
    ) -> Result<()> {
        let result = match Command::new(command.as_ref()).args(arguments).output() {
            Ok(r) => r,
            Err(e) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "Unable to execute the `{}` command: {}",
                        command.as_ref().to_string_lossy(),
                        e
                    ),
                ))
            }
        };

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let stdout = String::from_utf8_lossy(&result.stdout);

            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "The `{}` command failed with:\nstdout:\n{}\n\nstderr:\n{}\n",
                    command.as_ref().to_string_lossy(),
                    stdout,
                    stderr,
                ),
            ));
        }

        Ok({})
    }
}

#[cfg(test)]
mod test {
    use super::Process;
    use crate::{assert_error, expect, vec_deq, verify_expectations};
    use ::std::{
        cell::RefCell,
        collections::VecDeque,
        io::{Error, ErrorKind, Result},
    };

    struct TestExpectations {
        std_process_command_new: VecDeque<(&'static str, ())>,
        std_process_command_args: VecDeque<(Vec<&'static str>, ())>,
        std_process_command_output: VecDeque<((), Result<std::process::Output>)>,
        std_process_exit_status_success: VecDeque<((), bool)>,
    }

    impl TestExpectations {
        fn new() -> Self {
            TestExpectations {
                std_process_command_new: vec_deq![],
                std_process_command_args: vec_deq![],
                std_process_command_output: vec_deq![],
                std_process_exit_status_success: vec_deq![],
            }
        }
    }

    thread_local! { static TEST_EXPECTATIONS: RefCell<TestExpectations> = RefCell::new(TestExpectations::new()) }

    pub mod std {
        pub mod process {
            use super::super::TEST_EXPECTATIONS;
            use crate::verify_expectation;
            use std::{ffi::OsStr, io::Result};

            pub struct ExitStatus {}

            impl ExitStatus {
                pub fn success(&self) -> bool {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_exit_status_success =>
                            std::process::ExitStatus::success { _ }
                    )
                }
            }

            pub struct Output {
                pub status: ExitStatus,
                pub stdout: Vec<u8>,
                pub stderr: Vec<u8>,
            }

            pub struct Command {}

            impl Command {
                pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
                    verify_expectation!(TEST_EXPECTATIONS::std_process_command_new => std::process::Command::new
                        { program.as_ref().to_string_lossy() });

                    Self {}
                }

                pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(
                    &mut self,
                    args: I,
                ) -> &mut Self {
                    let mut result = vec![];
                    for arg in args {
                        result.push(String::from(arg.as_ref().to_string_lossy()));
                    }

                    verify_expectation!(TEST_EXPECTATIONS::std_process_command_args => std::process::Command::args { result });

                    self
                }

                pub fn output(&mut self) -> Result<Output> {
                    verify_expectation!(TEST_EXPECTATIONS::std_process_command_output => std::process::Command::output { _ })
                }
            }
        }
    }

    fn verify_expectations() {
        verify_expectations!(
            std::process::Command::new => TEST_EXPECTATIONS::std_process_command_new,
            std::process::Command::args => TEST_EXPECTATIONS::std_process_command_args,
            std::process::Command::output => TEST_EXPECTATIONS::std_process_command_output,
            std::process::ExitStatus::success => TEST_EXPECTATIONS::std_process_exit_status_success,
        );
    }

    #[test]
    fn oneshot_command_executes_process_successfully() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "ls" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec!["-la"] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_output: { _ => Ok(std::process::Output {
            status: std::process::ExitStatus {},
            stdout: "total 47\n\
                drwxr-xr-x 5 root root  3488 Oct 26 16:54 .\n\
                drwxr-xr-x 4 root root  3488 Jun  7 16:09 ..\n\
                ".as_bytes().to_vec(),
            stderr: vec![],
        }) });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_success: { _ => true });

        assert!(Process::oneshot("ls", &["-la"]).is_ok());

        verify_expectations();
    }

    #[test]
    fn oneshot_command_returns_error_if_process_execution_fails() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "lsl" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec!["-la"] => _ });
        expect!(
            TEST_EXPECTATIONS::std_process_command_output:
            { _ => Err(Error::new(ErrorKind::NotFound, format!("lsl: command not found\n"))) },
        );

        assert_error!(
            ErrorKind::Other,
            "Unable to execute the `lsl` command: lsl: command not found\n",
            Process::oneshot("lsl", &["-la"])
        );

        verify_expectations();
    }

    #[test]
    fn oneshot_command_returns_error_if_process_returns_non_zero_status() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "ls" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec!["/not_existent"] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_output: { _ => Ok(std::process::Output {
            status: std::process::ExitStatus {},
            stdout: vec![],
            stderr: "ls: cannot open directory '/non_existent': Permission denied\n".as_bytes().to_vec(),
        }) });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_success: { _ => false });

        assert_error!(
            ErrorKind::Other,
            "The `ls` command failed with:\nstdout:\n\n\nstderr:\nls: \
            cannot open directory \'/non_existent\': Permission denied\n\n",
            Process::oneshot("ls", &["/not_existent"])
        );

        verify_expectations();
    }
}
