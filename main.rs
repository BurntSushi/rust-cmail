use std::{
    env,
    ffi::OsString,
    io::{self, BufRead, Read, Write},
    process::{self, Command, ExitCode, Stdio},
    thread,
    time::Duration,
};

use anyhow::Context;
use crossbeam_channel::{self as channel, Receiver, Sender};
use libc::c_int;

/// The main starting point for cmail.
///
/// High level overview:
///
/// 1. Spawn the command given or read from stdin if no command is given.
/// 2. Start the email sender.
/// 3. Create a channel that ticks every N seconds.
/// 4. Start the main loop which waits on three channels: OS signals, the
///    ticker and lines read from the spawned command (or stdin).
fn main() -> anyhow::Result<ExitCode> {
    // We must start our signal notifier before spawning any threads!
    let signal = signal_notify(&[libc::SIGINT, libc::SIGTERM])?;
    let config = Config::parse(std::env::args_os())?;

    // When we don't have any arguments, cmail sends email containing stdin.
    let (mut cmd, lines, mut subject) =
        if let Some((ref bin, ref args, ref human_readable)) = config.cmd {
            let subject = human_readable.clone();
            let mut command = Command::new(bin);
            command.args(args);
            let (cmd, lines) = Cmd::run(command, !config.silent)?;
            (Some(cmd), lines, subject)
        } else {
            let passthru = Passthru::stdout(!config.silent);
            let stdin = passthru.gobble(io::stdin());
            (None, stdin, "<stdin>".to_string())
        };
    if let Some(ref explicit_subject) = config.subject {
        subject = explicit_subject.clone();
    }

    let email = match config.to {
        None => env::var("EMAIL").unwrap_or(String::new()),
        Some(ref s) => s.to_string(),
    };
    anyhow::ensure!(
        !email.is_empty(),
        "Please specify an email address with --to or by \
         setting the EMAIL environment variable."
    );
    let emailer = EmailSender::run(subject, email, config.send_all);

    // If period is zero, then ticker never ticks.
    let ticker = if config.period.is_zero() {
        channel::never()
    } else {
        channel::tick(config.period)
    };
    // Set to true if either the spawned process or a `sendmail` command
    // is interrupted. Setting this to `true` means we've initiated a graceful
    // shutdown of cmail that will culminate in one last email send.
    let mut killed = false;
    // Contains the next batch of lines to email. If the ticker is enabled,
    // then this is emptied at every tick.
    let mut outlines = Vec::with_capacity(1024);
    loop {
        crossbeam_channel::select! {
            // Respond to an OS signal. Currently, we just listen for
            // INT (^C) and TERM (kill).
            recv(signal) -> _ => {
                killed = true;
                if let Some(ref mut cmd) = cmd {
                    // If we're running a command and receive an interrupt,
                    // then we don't quit right away and send an email.
                    // Instead, we *ask* the child process to quit and we'll
                    // continue reading from its stdout/stderr until EOF.
                    //
                    // (Once EOF is hit, the `lines` channel is closed.)
                    cmd.kill()?;
                } else {
                    // .. on the other hand, if we're reading from stdin,
                    // then there's really nothing else we can do other than
                    // send what we've got and quit.
                    return emailer.last_send(cmd, outlines, killed);
                }
            }
            // When a tick happens, we just want to send the lines we've
            // accumulated so far and start over again.
            //
            // If, during the tick, the `sendmail` process is interrupted,
            // then we take this as a sign that we should quit.
            //
            // Finally, don't respond to ticks if we're shutting down.
            recv(ticker) -> _ => {
                if !killed {
                    killed = emailer.send(outlines)?;
                    outlines = Vec::with_capacity(1024);
                    match cmd {
                        Some(ref mut cmd) if killed => { cmd.kill()?; }
                        _ => {}
                    }
                }
            }
            // Receive a single line read from the spawned process (or stdin).
            // This simply adds the line to our `outlines` buffer.
            // Something interesting only happens when the channel is closed:
            // we send one last email with the lines we've accumulated.
            //
            // N.B. This is the main exit point of cmail under normal
            // operation. In the absence of ticks, this is usually the only
            // channel that gets any activity!
            recv(lines) -> line => match line.ok() {
                Some(line) => outlines.push(line?),
                None => return emailer.last_send(cmd, outlines, killed),
            },
        }
    }
}

/// An email sender collects groups of lines and sends emails concurrently.
struct EmailSender {
    /// The email sender listens on this channel for sequences of lines.
    send_lines: Sender<Vec<String>>,
    /// When a sequence of lines has been emailed (either successfully or
    /// unsuccessfully), the result is sent on this channel.
    ///
    /// In particular, the next email is not attempted until a consumer
    /// receives the corresponding result on this channel.
    recv_result: Receiver<io::Result<bool>>,
    /// Closed when the email sender shuts down.
    recv_done: Receiver<()>,
}

impl EmailSender {
    /// Creates a new email sender.
    ///
    /// This spawns a thread responsible for sending lines read from the
    /// running command to the email address provided. The value returned
    /// contains several channels that can be used to interact with this
    /// thread. Instead of using the channels explicitly, you should prefer
    /// to use the methods defined below.
    fn run(subject: String, email: String, send_all: bool) -> EmailSender {
        let mut to_send: Vec<String> = Vec::with_capacity(1024);
        let (send_lines, recv_lines) = channel::bounded::<Vec<String>>(0);
        let (send_result, recv_result) = channel::bounded(0);
        let (send_done, recv_done) = channel::bounded(1);
        thread::spawn(move || {
            let mut interrupted = false;
            for lines in recv_lines {
                if send_all {
                    to_send.extend(lines);
                } else {
                    if lines.len() == 0 {
                        to_send = vec!["No output.".to_owned()];
                    } else {
                        to_send = lines;
                    }
                }
                let result = if interrupted {
                    email_lines(&subject, &email, &to_send)
                        .map(|_| interrupted)
                } else {
                    let r = email_lines_retry(&subject, &email, &to_send);
                    interrupted = *r.as_ref().unwrap_or(&false);
                    r
                };
                send_result.send(result).unwrap();
            }
            // unblock recv_done
            drop(send_done);
        });
        EmailSender { send_lines, recv_result, recv_done }
    }

    /// Consume the email sender and send one last batch of lines.
    ///
    /// If this method completes successfully, then the email sender thread
    /// will have shut down, the last email will have been sent and the spawned
    /// child process (if one exists) will be reaped.
    ///
    /// If cmail was run with a command, then `cmd` should be that command.
    /// Otherwise, it should be `None` when cmail reads from stdin.
    ///
    /// `killed` is a bool indicating whether any of the child processes
    /// spawned by cmail were killed by a signal. When `killed` is true,
    /// a non-zero exit code is returned in the result. Otherwise, a zero
    /// exit code is returned.
    fn last_send(
        self,
        cmd: Option<Cmd>,
        mut lines: Vec<String>,
        killed: bool,
    ) -> anyhow::Result<ExitCode> {
        let int = match cmd {
            None => killed,
            Some(mut cmd) => !cmd.wait()?.success() || killed,
        };
        let msg = if int {
            "Program interrupted."
        } else {
            "Program completed successfully."
        };
        lines.extend(vec!["", "", msg].into_iter().map(str::to_owned));
        self.send(lines)?;
        self.done();
        Ok(if killed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
    }

    /// Sends a sequence of lines.
    ///
    /// If this method completes successfully, then an email will have been
    /// sent containing the lines given.
    ///
    /// If an interrupt occurred when trying to send mail, then `true` is
    /// returned in the result. Otherwise, `false` is returned. (This
    /// corresponds to the `killed` parameter in `last_send`. It should also
    /// be used to start a graceful shutdown of cmail.)
    fn send(&self, lines: Vec<String>) -> anyhow::Result<bool> {
        self.send_lines.send(lines).unwrap();
        Ok(self.recv_result.recv().unwrap()?)
    }

    /// Start a graceful shutdown of the emailing thread and wait for all
    /// remaining lines to be sent.
    fn done(self) {
        // Shut down the thread responsible for sending emails.
        drop(self.send_lines);
        // Wait for it to finished.
        let _ = self.recv_done.recv();
    }
}

/// A simple convenience for handling the command that cmail is watching.
#[derive(Debug)]
struct Cmd {
    child: process::Child,
}

impl Cmd {
    /// Run the given command (where each item in `cmd` is a single argument).
    ///
    /// If `passthru` is true, then the stdout/stderr of the command is printed
    /// on the stdout/stderr of cmail.
    ///
    /// This returns a tuple. The first value is the `Cmd` abstraction, which
    /// can be killed and reaped. The second value is a channel that receives
    /// line results from the corresponding child process. The channel is
    /// closed when the child's stdout and stderr emit EOF.
    fn run(
        mut command: Command,
        passthru: bool,
    ) -> anyhow::Result<(Cmd, Receiver<io::Result<String>>)> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout = Passthru::stdout(passthru).gobble(stdout);
        let stderr = Passthru::stderr(passthru).gobble(stderr);
        Ok((Cmd { child }, muxer(vec![stdout, stderr])))
    }

    /// Kill this command and wait to reap it.
    fn kill(&mut self) -> anyhow::Result<process::ExitStatus> {
        // Ignore the error here, in case the child has already died.
        // We simply do not care if `kill` fails.
        let _ = self.child.kill();
        self.wait()
    }

    /// Block until the child is reaped.
    fn wait(&mut self) -> anyhow::Result<process::ExitStatus> {
        Ok(self.child.wait()?)
    }
}

/// Passthru describes how to pass the command's output through the cmail
/// process.
#[derive(Clone, Copy, Debug)]
enum Passthru {
    No,
    Stdout,
    Stderr,
}

impl Passthru {
    /// Pass through on stdout if `yes` is true.
    fn stdout(yes: bool) -> Passthru {
        if yes {
            Passthru::Stdout
        } else {
            Passthru::No
        }
    }

    /// Pass through on stderr if `yes` is true.
    fn stderr(yes: bool) -> Passthru {
        if yes {
            Passthru::Stderr
        } else {
            Passthru::No
        }
    }

    /// Create a writer corresponding to the pass through settings.
    ///
    /// If there's no pass through, then a /dev/null-like writer is returned.
    fn wtr(self) -> Box<dyn io::Write> {
        match self {
            Passthru::No => Box::new(io::sink()),
            Passthru::Stdout => Box::new(io::stdout()),
            Passthru::Stderr => Box::new(io::stderr()),
        }
    }

    /// Read lines on `rdr` and send the *result* along the channel returned,
    ///
    /// This will also apply the pass through settings in `self`.
    fn gobble<R>(self, rdr: R) -> Receiver<io::Result<String>>
    where
        R: Read + Send + 'static,
    {
        let (s, r) = channel::bounded(0);
        thread::spawn(move || {
            let mut wtr = self.wtr();
            for line in io::BufReader::new(rdr).lines() {
                if let Ok(ref line) = line {
                    writeln!(&mut wtr, "{}", line).unwrap();
                }
                s.send(line).unwrap();
            }
        });
        r
    }
}

#[derive(Debug, Default)]
struct Config {
    /// If a command to run is given, then we'll get a binary all of its
    /// arguments (possibly empty). The `String` is just a human readable
    /// (possibly lossily decoded) version of the command.
    cmd: Option<(OsString, Vec<OsString>, String)>,
    period: Duration,
    silent: bool,
    send_all: bool,
    to: Option<String>,
    subject: Option<String>,
}

impl Config {
    /// Parse the given OS string args into a `Config`.
    fn parse<I>(args: I) -> anyhow::Result<Config>
    where
        I: IntoIterator<Item = OsString> + 'static,
    {
        use lexopt::{Arg::*, ValueExt};

        const USAGE: &str = r#"
Usage: cmail [options] [<arg> ...]

Options:
    -h, --help             Display this help message.
    -p ARG, --period ARG   Data is emailed at this period.
                           Set to `0s` to disable and send only one email
                           when the command completes. Defaults to 15m.
                           Example: `2h30m` or `2 hours, 30 minutes`.
    -s, --silent           Don't pass the command's stdout/stderr to the
                           terminal. Instead, only send stdout/stderr
                           in email.
    -a, --send-all         Send the entire command's output on each email.
                           N.B. This saves all output in memory.
    -t ARG, --to ARG       The email address to send to. By default, this
                           is set to $EMAIL. If neither $EMAIL nor --to
                           are set, then an error is returned.
    --subject ARG          Forcefully set the subject of the email.
"#;

        let mut config = Config::default();
        let mut parser = lexopt::Parser::from_iter(args);
        let mut cmd_bin_and_args = vec![];
        while let Some(arg) = parser.next()? {
            match arg {
                Short('h') | Long("help") => {
                    anyhow::bail!(USAGE);
                }
                Short('p') | Long("period") => {
                    let span: jiff::Span = parser.value()?.parse()?;
                    let sdur = span.to_jiff_duration(&jiff::Zoned::now())?;
                    config.period = sdur.try_into().with_context(|| {
                        format!(
                            "failed to convert `{span:#}` into unsiged \
                             duration",
                        )
                    })?;
                }
                Short('s') | Long("silent") => {
                    config.silent = true;
                }
                Short('a') | Long("send-all") => {
                    config.send_all = true;
                }
                Short('t') | Long("to") => {
                    config.to = Some(parser.value()?.parse()?);
                }
                Long("subject") => {
                    config.subject = Some(parser.value()?.parse()?);
                }
                Value(cmd_arg) => {
                    cmd_bin_and_args.push(cmd_arg);
                }
                _ => return Err(arg.unexpected().into()),
            }
        }
        if !cmd_bin_and_args.is_empty() {
            let bin = cmd_bin_and_args.remove(0);
            let mut human_readable = vec![bin.to_string_lossy().into_owned()];
            for arg in cmd_bin_and_args.iter() {
                human_readable.push(arg.to_string_lossy().into_owned());
            }
            config.cmd =
                Some((bin, cmd_bin_and_args, human_readable.join(" ")));
        }
        Ok(config)
    }
}

/// Given a sequence of receiving channels, multiplex them into one.
///
/// This spawns a thread for each element in `inps` and sends them all on to
/// a single channel.
///
/// The resulting channel is closed only when all given channels in `inps`
/// have been closed.
fn muxer<T: Send + 'static>(inps: Vec<Receiver<T>>) -> Receiver<T> {
    // If a command sends a lot of output to stdout/stderr in a short time
    // period, then setting a large buffer here on the channel gives us a
    // little wiggle room to keep up with it.
    let (s, r) = channel::bounded(5000);
    for inp in inps {
        let s = s.clone();
        thread::spawn(move || {
            for item in inp {
                s.send(item).unwrap();
            }
        });
    }
    r
}

/// Sends an email containing `lines` to `email` for the command `cmd`.
///
/// If the child `sendmail` process was interrupted, then sending the email
/// is retried exactly once.
///
/// If a retry occurred, then `true` is returned inside the result. Otherwise,
/// `false` is returned.
fn email_lines_retry(
    subject: &str,
    email: &str,
    to_send: &[String],
) -> io::Result<bool> {
    // If the first call to email_lines fails because of an
    // interruption, then we try to send once more.
    // This is to permit the use of ^C in the terminal. The
    // intended effect is to stop the running process and email
    // whatever has been accumulated. But if `sendmail` is running
    // when ^C is sent, then the command fails and no mail is sent.
    // So we try once more: if that produces an error, we give up.
    match email_lines(subject, email, to_send) {
        Ok(()) => Ok(false),
        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
            // If we fail again for any reason, bubble up the
            // error and notify the receiver that we should quit.
            // This lets the user slam on ^C twice in a row
            // to really quit. :]
            email_lines(subject, email, to_send).map(|_| true)
        }
        Err(e) => Err(e),
    }
}

/// Sends an email containing `lines` to `email` for the command `cmd`.
fn email_lines(
    subject: &str,
    email: &str,
    lines: &[String],
) -> io::Result<()> {
    let mut child =
        Command::new("sendmail").arg("-t").stdin(Stdio::piped()).spawn()?;
    let subject: String = subject.chars().take(50).collect();
    let sep: String = ::std::iter::repeat('-').take(79).collect();
    {
        // Open a new scope here since `buf` borrows `child.stdin` mutably.
        // We need to drop this borrow before calling `child.wait()`, which
        // also borrows `child` mutably.
        let mut buf = io::BufWriter::new(child.stdin.as_mut().unwrap());
        writeln!(
            &mut buf,
            "\
Subject: [cmail] {subject}
From: {email}
To: {email}
",
        )?;
        // Add some extra fluff to make it clear what command is being run.
        writeln!(&mut buf, "{sep}\n{subject}\n{sep}")?;
        for line in lines {
            writeln!(&mut buf, "{line}")?;
        }
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        // If the exit code is `None`, then we infer that the `sendmail`
        // process was killed by a signal.
        // In typical usage, this means the user pressed ^C on their terminal,
        // rather than it being indicative of some other reason why sendmail
        // won't work.
        // We use this information to retry the email send (but are careful
        // to only retry once).
        Err(match status.code() {
            None => io::Error::new(
                io::ErrorKind::Interrupted,
                "email send interrupted",
            ),
            Some(_) => {
                io::Error::new(io::ErrorKind::Other, status.to_string())
            }
        })
    }
}

fn signal_notify(signals: &[c_int]) -> anyhow::Result<Receiver<c_int>> {
    let (s, r) = channel::bounded(100);
    let mut signals = signal_hook::iterator::Signals::new(signals)?;
    thread::spawn(move || {
        for signal in signals.forever() {
            s.send(signal).unwrap();
        }
    });
    Ok(r)
}
