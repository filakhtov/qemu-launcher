use crate::qmp::QmpPipe;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::{Error, ErrorKind, Read, Result, Write},
};
#[cfg(not(test))]
use std::{
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};
#[cfg(test)]
use test::std::process::{Child, Command, Stdio};

pub struct StdioReadWrite<'a> {
    stdin: &'a mut dyn Write,
    stdout: &'a mut dyn Read,
}

impl<'a> StdioReadWrite<'a> {
    pub fn new(stdin: &'a mut impl Write, stdout: &'a mut impl Read) -> Self {
        Self {
            stdin: stdin,
            stdout: stdout,
        }
    }
}

impl Read for StdioReadWrite<'_> {
    fn read(&mut self, message: &mut [u8]) -> Result<usize> {
        self.stdout.read(message)
    }
}

impl Write for StdioReadWrite<'_> {
    fn write(&mut self, message: &[u8]) -> Result<usize> {
        self.stdin.write(message)
    }

    fn flush(&mut self) -> Result<()> {
        self.stdin.flush()
    }
}

impl QmpPipe for StdioReadWrite<'_> {}

pub struct ChildProcess {
    child: Child,
}

impl ChildProcess {
    pub fn wait(mut self) -> Result<()> {
        match self.child.wait() {
            Ok(r) => match r.success() {
                true => Ok({}),
                false => Err(match r.code() {
                    Some(c) => Error::new(
                        ErrorKind::Other,
                        format!("The child process was terminated with `{}` status.", c),
                    ),
                    None => Error::new(
                        ErrorKind::Other,
                        "The child process terminated unsuccessfully, but did not return the exit status."
                    ),
                }),
            },
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!("The child process failed: {}", e),
            )),
        }
    }

    pub fn get_stdio(&mut self) -> Result<StdioReadWrite> {
        let stdin = match self.child.stdin.as_mut() {
            Some(stdin) => stdin,
            None => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "Unable to get the child process stdin",
                ))
            }
        };
        let stdout = match self.child.stdout.as_mut() {
            Some(stdout) => stdout,
            None => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "Unable to get the child process stdout",
                ))
            }
        };

        Ok(StdioReadWrite::new(stdin, stdout))
    }
}

pub struct Process {
    command: OsString,
    arguments: Vec<OsString>,
    env_clear: bool,
    uid: Option<u32>,
    gid: Option<u32>,
    envs: HashMap<OsString, OsString>,
}

impl Process {
    pub fn new<C: AsRef<OsStr>>(command: C) -> Self {
        Self {
            command: command.as_ref().to_owned(),
            arguments: vec![],
            env_clear: false,
            uid: None,
            gid: None,
            envs: HashMap::new(),
        }
    }

    pub fn set_args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Self {
        self.arguments = vec![];

        for arg in args {
            self.arguments.push(arg.as_ref().to_owned());
        }

        self
    }

    pub fn should_clear_env(mut self, should_clear: bool) -> Self {
        self.env_clear = should_clear;

        self
    }

    pub fn set_effective_user_id(mut self, uid: &Option<u16>) -> Self {
        if let Some(uid) = uid {
            self.uid = Some(*uid as u32);
        }

        self
    }

    pub fn set_effective_group_id(mut self, gid: &Option<u16>) -> Self {
        if let Some(gid) = gid {
            self.gid = Some(*gid as u32);
        }

        self
    }

    pub fn set_environment_variables<
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    >(
        mut self,
        vars: I,
    ) -> Self {
        self.envs = HashMap::new();

        for var in vars {
            self.envs
                .insert(var.0.as_ref().to_owned(), var.1.as_ref().to_owned());
        }

        self
    }

    pub fn spawn(self) -> Result<ChildProcess> {
        let mut command = Command::new(self.command.as_os_str());
        command
            .args(self.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());

        if self.env_clear {
            command.env_clear();
        }

        if let Some(uid) = self.uid {
            command.uid(uid);
        }

        if let Some(gid) = self.gid {
            command.gid(gid);
        }

        if self.envs.len() > 0 {
            command.envs(self.envs);
        }

        let child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("Failed to spawn child process: {}", e),
                ))
            }
        };

        Ok(ChildProcess { child })
    }

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
    use self::std::process::{Child, ChildStdin, ChildStdout, ExitStatus, Stdio};
    use super::{ChildProcess, Process};
    use crate::{assert_error, expect, vec_deq, verify_expectations};
    use ::std::{
        cell::RefCell,
        collections::VecDeque,
        io::{Error, ErrorKind, Result},
    };

    struct TestExpectations {
        std_process_child_wait: VecDeque<((), Result<ExitStatus>)>,
        std_process_command_args: VecDeque<(Vec<&'static str>, ())>,
        std_process_command_env_clear: VecDeque<((), ())>,
        std_process_command_envs: VecDeque<(Vec<(String, String)>, ())>,
        std_process_command_gid: VecDeque<(u32, ())>,
        std_process_command_new: VecDeque<(&'static str, ())>,
        std_process_command_output: VecDeque<((), Result<std::process::Output>)>,
        std_process_command_spawn: VecDeque<((), Result<Child>)>,
        std_process_command_stdin: VecDeque<(std::process::Stdio, ())>,
        std_process_command_stdout: VecDeque<(std::process::Stdio, ())>,
        std_process_command_uid: VecDeque<(u32, ())>,
        std_process_exit_status_code: VecDeque<((), Option<i32>)>,
        std_process_exit_status_success: VecDeque<((), bool)>,
    }

    impl TestExpectations {
        fn new() -> Self {
            TestExpectations {
                std_process_child_wait: vec_deq![],
                std_process_command_args: vec_deq![],
                std_process_command_env_clear: vec_deq![],
                std_process_command_envs: vec_deq![],
                std_process_command_gid: vec_deq![],
                std_process_command_new: vec_deq![],
                std_process_command_output: vec_deq![],
                std_process_command_spawn: vec_deq![],
                std_process_command_stdin: vec_deq![],
                std_process_command_stdout: vec_deq![],
                std_process_command_uid: vec_deq![],
                std_process_exit_status_code: vec_deq![],
                std_process_exit_status_success: vec_deq![],
            }
        }
    }

    thread_local! { static TEST_EXPECTATIONS: RefCell<TestExpectations> = RefCell::new(TestExpectations::new()) }

    pub mod std {
        pub mod process {
            use super::super::TEST_EXPECTATIONS;
            use crate::verify_expectation;
            use std::{
                cmp::PartialEq,
                ffi::OsStr,
                io::{Read, Result, Write},
            };

            pub struct ExitStatus {}

            impl ExitStatus {
                pub fn success(&self) -> bool {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_exit_status_success =>
                            std::process::ExitStatus::success { _ }
                    )
                }

                pub fn code(&self) -> Option<i32> {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_exit_status_code =>
                            std::process::ExitStatus::code { _ }
                    )
                }
            }

            pub struct Output {
                pub status: ExitStatus,
                pub stdout: Vec<u8>,
                pub stderr: Vec<u8>,
            }

            #[derive(Debug)]
            pub struct Stdio {
                t: &'static str,
            }

            impl Stdio {
                pub fn piped() -> Self {
                    Self { t: "piped" }
                }
            }

            impl PartialEq for Stdio {
                fn eq(&self, rhs: &Self) -> bool {
                    self.t == rhs.t
                }
            }

            pub struct ChildStdin {}

            impl Write for ChildStdin {
                fn write(&mut self, _: &[u8]) -> Result<usize> {
                    panic!("Unexpected call to std::process::ChildStdin::write() method.")
                }

                fn flush(&mut self) -> Result<()> {
                    panic!("Unexpected call to std::process::ChildStdin::flush() method.")
                }
            }

            pub struct ChildStdout {}

            impl Read for ChildStdout {
                fn read(&mut self, _: &mut [u8]) -> Result<usize> {
                    panic!("Unexpected call to std::process::ChildStdout::read() method.")
                }
            }

            pub struct Child {
                pub stdin: Option<ChildStdin>,
                pub stdout: Option<ChildStdout>,
            }

            impl Child {
                pub fn wait(&mut self) -> Result<ExitStatus> {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_child_wait => std::process::Child::wait { _ }
                    )
                }
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

                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_args => std::process::Command::args { result }
                    );

                    self
                }

                pub fn output(&mut self) -> Result<Output> {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_output => std::process::Command::output { _ }
                    )
                }

                pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
                    let cfg: Stdio = cfg.into();
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_stdin => std::process::Command::stdin { cfg }
                    );

                    self
                }

                pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
                    let cfg: Stdio = cfg.into();
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_stdout => std::process::Command::stdout { cfg }
                    );

                    self
                }

                pub fn env_clear(&mut self) -> &mut Self {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_env_clear =>
                            std::process::Command::env_clear { _ }
                    );

                    self
                }

                pub fn envs<I: IntoIterator<Item = (K, V)>, K: AsRef<OsStr>, V: AsRef<OsStr>>(
                    &mut self,
                    vars: I,
                ) -> &mut Self {
                    let mut a = vec![];
                    for (k, v) in vars {
                        a.push((
                            String::from(k.as_ref().to_string_lossy()),
                            String::from(v.as_ref().to_string_lossy()),
                        ));
                    }
                    a.sort();
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_envs => std::process::Command::envs { a }
                    );

                    self
                }

                pub fn spawn(&mut self) -> Result<Child> {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_spawn => std::process::Command::spawn { _ }
                    )
                }

                pub fn uid(&mut self, id: u32) -> &mut Self {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_uid => std::process::Command::uid { id }
                    );

                    self
                }

                pub fn gid(&mut self, id: u32) -> &mut Self {
                    verify_expectation!(
                        TEST_EXPECTATIONS::std_process_command_gid => std::process::Command::gid { id }
                    );

                    self
                }
            }
        }
    }

    macro_rules! error {
        ($msg:expr) => {{
            Err(Error::new(ErrorKind::Other, format!("{}", $msg)))
        }};
    }

    fn verify_expectations() {
        verify_expectations!(
            std::process::Child::wait => TEST_EXPECTATIONS::std_process_child_wait,
            std::process::Command::args => TEST_EXPECTATIONS::std_process_command_args,
            std::process::Command::env_clear => TEST_EXPECTATIONS::std_process_command_env_clear,
            std::process::Command::envs => TEST_EXPECTATIONS::std_process_command_envs,
            std::process::Command::gid => TEST_EXPECTATIONS::std_process_command_gid,
            std::process::Command::new => TEST_EXPECTATIONS::std_process_command_new,
            std::process::Command::output => TEST_EXPECTATIONS::std_process_command_output,
            std::process::Command::spawn => TEST_EXPECTATIONS::std_process_command_spawn,
            std::process::Command::stdin => TEST_EXPECTATIONS::std_process_command_stdin,
            std::process::Command::stdout => TEST_EXPECTATIONS::std_process_command_stdout,
            std::process::Command::uid => TEST_EXPECTATIONS::std_process_command_uid,
            std::process::ExitStatus::code => TEST_EXPECTATIONS::std_process_exit_status_code,
            std::process::ExitStatus::success => TEST_EXPECTATIONS::std_process_exit_status_success,
        );
    }

    #[test]
    fn process_oneshot_command_executes_process_successfully() {
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
    fn process_oneshot_command_returns_error_if_process_execution_fails() {
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
    fn process_oneshot_command_returns_error_if_process_returns_non_zero_status() {
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

    #[test]
    fn process_new_returns_process_instance() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test");

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_args_sets_command_line_options_for_child_process() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec!["-c", "test.yml"] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test").set_args(&["-c", "test.yml"]);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_args_replaces_command_line_options_for_child_process_on_subsequent_calls() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec!["-c", "prod.yml"] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test")
            .set_args(&["-c", "test.yml"])
            .set_args(&["-c", "prod.yml"]);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_should_clear_env_true_clears_child_process_environment() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_env_clear: { _ => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test").should_clear_env(true);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_should_clear_env_respects_last_configured_value() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test")
            .should_clear_env(true)
            .should_clear_env(false);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_effective_user_id_sets_effective_user_id_for_child_process_if_some_is_given() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-user" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_uid: { 123 => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-user").set_effective_user_id(&Some(123));

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_effective_user_does_nothing_if_none_is_given() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-user" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-user").set_effective_user_id(&None);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_effective_group_sets_effective_group_id_for_child_process_if_some_is_given() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-group" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_gid: { 321 => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-group").set_effective_group_id(&Some(321));

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_effective_group_does_nothing_if_none_is_given() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-group" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-group").set_effective_group_id(&None);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_environment_variables_adds_additional_child_process_environment() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-group" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(
            TEST_EXPECTATIONS::std_process_command_envs:
            { vec![("ENV".to_string(), "var".to_string()), ("VAR".to_string(), "2".to_string())] => _ }
        );
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-group")
            .set_environment_variables(vec![("ENV", "var"), ("VAR", "2")]);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_set_environment_variables_replaces_child_process_variables_on_subsequent_calls() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "test-group" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(
            TEST_EXPECTATIONS::std_process_command_envs: { vec![("TEST".to_string(), "true".to_string())] => _ },
        );
        expect!(TEST_EXPECTATIONS::std_process_command_spawn: { _ => Ok(Child {
            stdin: Some(ChildStdin {}),
            stdout: Some(ChildStdout {}),
        }) });

        let subject = Process::new("test-group")
            .set_environment_variables(vec![("ENV", "var"), ("DEMO", "false")])
            .set_environment_variables(vec![("TEST", "true")]);

        assert!(subject.spawn().is_ok());

        verify_expectations();
    }

    #[test]
    fn process_spawn_returns_error_if_unable_to_start_child_process() {
        expect!(TEST_EXPECTATIONS::std_process_command_new: { "failed" => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_args: { vec![] => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdin: { Stdio::piped() => _ });
        expect!(TEST_EXPECTATIONS::std_process_command_stdout: { Stdio::piped() => _ });
        expect!(
            TEST_EXPECTATIONS::std_process_command_spawn:
            { _ => Err(::std::io::Error::new(ErrorKind::Other, "test error")) }
        );

        let subject = Process::new("failed");

        assert_error!(
            ErrorKind::Other,
            "Failed to spawn child process: test error",
            subject.spawn()
        );

        verify_expectations();
    }

    #[test]
    fn child_process_wait_returns_ok_if_child_exits_successfully() {
        expect!(TEST_EXPECTATIONS::std_process_child_wait: { _ => Ok(ExitStatus {}) });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_success: { _ => true });

        let subject = ChildProcess {
            child: Child {
                stdin: Some(ChildStdin {}),
                stdout: Some(ChildStdout {}),
            },
        };

        assert!(subject.wait().is_ok());

        verify_expectations();
    }

    #[test]
    fn child_process_wait_returns_error_if_child_wait_fails() {
        expect!(TEST_EXPECTATIONS::std_process_child_wait: { _ => error!("test error") });

        let subject = ChildProcess {
            child: Child {
                stdin: Some(ChildStdin {}),
                stdout: Some(ChildStdout {}),
            },
        };

        assert_error!(
            ErrorKind::Other,
            "The child process failed: test error",
            subject.wait()
        );

        verify_expectations();
    }

    #[test]
    fn child_process_wait_returns_error_if_child_fails_and_returns_exit_status() {
        expect!(TEST_EXPECTATIONS::std_process_child_wait: { _ => Ok(ExitStatus {}) });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_success: { _ => false });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_code: { _ => Some(3) });

        let subject = ChildProcess {
            child: Child {
                stdin: Some(ChildStdin {}),
                stdout: Some(ChildStdout {}),
            },
        };

        assert_error!(
            ErrorKind::Other,
            "The child process was terminated with `3` status.",
            subject.wait()
        );

        verify_expectations();
    }

    #[test]
    fn child_process_wait_returns_error_if_child_fails_and_returns_no_exit_status() {
        expect!(TEST_EXPECTATIONS::std_process_child_wait: { _ => Ok(ExitStatus {}) });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_success: { _ => false });
        expect!(TEST_EXPECTATIONS::std_process_exit_status_code: { _ => None });

        let subject = ChildProcess {
            child: Child {
                stdin: Some(ChildStdin {}),
                stdout: Some(ChildStdout {}),
            },
        };

        assert_error!(
            ErrorKind::Other,
            "The child process terminated unsuccessfully, but did not return the exit status.",
            subject.wait()
        );

        verify_expectations();
    }
}
