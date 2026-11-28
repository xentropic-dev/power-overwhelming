use anyhow::{Context, Result, ensure};
use application::talk::LogFilter;
use clap::{Args, Subcommand};
use domain::talk::{MAX_BODY_BYTES, Message, MessageId, Recipient, ThreadId, envelope};
use infrastructure::client::{
    self,
    talk::{self, Caller},
};
use serde_json::json;
use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};

const DEFAULT_TIMEOUT: u64 = 600;

#[derive(Args)]
pub struct TalkArgs {
    /// Box ID, unique prefix, shortcut, or repository name.
    #[arg(long = "box", global = true, value_name = "SELECTOR")]
    box_sel: Option<String>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Send to an agent, role, user, or run's active workers.
    Send {
        /// Recipient followed by message text. Use - for stdin. With --run, omit the
        /// recipient to broadcast, or specify task:<id> to address a single worker.
        #[arg(value_name = "TO_OR_MESSAGE")]
        words: Vec<String>,
        /// Explicit recipient when using --run.
        #[arg(long, value_name = "ADDRESS")]
        to: Option<String>,
        #[arg(long, value_name = "RUN")]
        run: Option<String>,
        #[arg(long)]
        urgent: bool,
        #[command(flatten)]
        input: InputOptions,
    },
    /// Ask the user a question with fixed answers. It appears in the user inbox of every box.
    Ask {
        /// Recipient (only `user`) followed by the question. Use - for stdin.
        #[arg(value_name = "USER_AND_QUESTION")]
        words: Vec<String>,
        /// Comma-separated answers, such as "Approve,Deny".
        #[arg(long, value_delimiter = ',', required = true)]
        choices: Vec<String>,
        /// Close the request unanswered after this many seconds.
        #[arg(long, value_name = "SECS")]
        expires: Option<u64>,
        #[command(flatten)]
        input: InputOptions,
    },
    /// List decision requests to the user from every box.
    #[command(hide = true)]
    Decisions {
        #[arg(long)]
        all: bool,
    },
    /// Reply in the thread containing a message.
    Reply {
        id: String,
        #[arg(value_name = "MESSAGE")]
        words: Vec<String>,
        #[command(flatten)]
        input: InputOptions,
    },
    /// Read incoming messages.
    Inbox {
        #[arg(long)]
        peek: bool,
        #[arg(long)]
        all: bool,
    },
    /// Display the thread containing a message.
    Show { id: String },
    /// Wait for the first reply in one or more threads.
    Wait {
        #[arg(required = true, value_name = "THREAD")]
        threads: Vec<String>,
        /// Accept only a reply to this message. Repeat for several messages.
        #[arg(long = "reply-to", value_name = "MESSAGE")]
        reply_to: Vec<String>,
        #[arg(long, default_value_t = DEFAULT_TIMEOUT, value_name = "SECS")]
        timeout: u64,
    },
    /// Display talk history as a coordinator or the user.
    Log {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        run: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(hide = true)]
    Ready { event: Option<String> },
    #[command(hide = true)]
    Busy,
}

#[derive(Args)]
struct InputOptions {
    /// Read the message from a UTF-8 file.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    #[arg(long)]
    wait: bool,
    #[arg(long, requires = "wait", value_name = "SECS")]
    timeout: Option<u64>,
}

pub fn run(args: TalkArgs) -> Result<()> {
    let caller = Caller {
        session: super::caller_session(),
        box_sel: args.box_sel,
        desktop: false,
    };
    let json = args.json;
    if !matches!(args.command, Action::Ready { .. } | Action::Busy) {
        client::ensure_daemon()?;
    }
    match args.command {
        Action::Send {
            words,
            to,
            run,
            urgent,
            input,
        } => {
            let (to, words) = recipient_and_body(words, to, run.is_some(), input.file.is_some())?;
            let body = body(words, input.file)?;
            let messages = talk::send(caller.clone(), to, run, body, urgent)?;
            sent(caller, messages, input.wait, input.timeout, json)
        }
        Action::Ask {
            words,
            choices,
            expires,
            input,
        } => {
            let mut words = words.into_iter();
            ensure!(
                words.next().as_deref() == Some("user"),
                "Decision requests go to the user: power talk ask user --choices A,B <question>"
            );
            let body = body(words.collect(), input.file)?;
            let expires = expires
                .map(|secs| secs.checked_mul(1000).context("Expiry is too large"))
                .transpose()?;
            let message = talk::ask(caller.clone(), body, choices, expires)?;
            sent(caller, vec![message], input.wait, input.timeout, json)
        }
        Action::Decisions { all } => print_messages(talk::decisions(caller, all)?, json, false),
        Action::Reply { id, words, input } => {
            let body = body(words, input.file)?;
            let message = talk::reply(caller.clone(), MessageId(id), body, None)?;
            sent(caller, vec![message], input.wait, input.timeout, json)
        }
        Action::Inbox { peek, all } => print_messages(talk::inbox(caller, all, peek)?, json, false),
        Action::Show { id } => {
            print_messages(talk::show(caller, MessageId(id), false)?, json, true)
        }
        Action::Log { agent, run, limit } => print_messages(
            talk::log(
                caller,
                LogFilter {
                    agent,
                    run,
                    limit: Some(limit),
                    ..LogFilter::default()
                },
            )?,
            json,
            true,
        ),
        Action::Wait {
            threads,
            reply_to,
            timeout,
        } => {
            let threads: Vec<_> = threads.into_iter().map(ThreadId).collect();
            let reply_to: Vec<_> = reply_to.into_iter().map(MessageId).collect();
            let reply = talk::wait(caller, threads.clone(), reply_to.clone(), timeout)?;
            wait_output(&threads, &reply_to, reply, json)
        }
        Action::Ready { event } => {
            let from_notify = event.is_some();
            if let Some(event) = event {
                let event: serde_json::Value =
                    serde_json::from_str(&event).context("Read turn event")?;
                if event["type"] != "agent-turn-complete" {
                    return Ok(());
                }
            }
            report_turn(caller, true, from_notify, json)
        }
        Action::Busy => report_turn(caller, false, false, json),
    }
}

fn report_turn(caller: Caller, ready: bool, from_notify: bool, json: bool) -> Result<()> {
    talk::turn(caller, ready, from_notify)?;
    if json {
        super::print_json(json!({"ok": true}))?;
    }
    Ok(())
}

fn recipient_and_body(
    words: Vec<String>,
    to: Option<String>,
    has_run: bool,
    has_file: bool,
) -> Result<(Option<String>, Vec<String>)> {
    if to.is_some() {
        return Ok((to, words));
    }
    let with_task = words.first().is_some_and(|word| word.starts_with("task:"))
        && (has_file || words.len() > 1);
    if has_run && !with_task {
        return Ok((None, words));
    }
    let mut words = words.into_iter();
    let target = words.next().context("Specify a talk recipient or --run")?;
    Ok((Some(target), words.collect()))
}

fn body(words: Vec<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(path) = file {
        ensure!(words.is_empty(), "Use either --file or message text");
        return read_body(File::open(&path).with_context(|| format!("Open {}", path.display()))?);
    }
    if words == ["-"] {
        return read_body(io::stdin().lock());
    }
    ensure!(
        !words.is_empty(),
        "Specify a message, --file or - for stdin"
    );
    Ok(words.join(" "))
}

fn read_body(input: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    input
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_BODY_BYTES,
        "Talk message body exceeds the 16 KiB ({MAX_BODY_BYTES} bytes) limit"
    );
    String::from_utf8(bytes).context("Talk messages must be UTF-8")
}

fn sent(
    caller: Caller,
    messages: Vec<Message>,
    wait: bool,
    timeout: Option<u64>,
    json: bool,
) -> Result<()> {
    let threads: Vec<_> = messages
        .iter()
        .map(|message| message.thread.clone())
        .collect();
    // Only a reply to one of these messages ends the wait, even if the thread has others.
    let reply_to: Vec<_> = messages.iter().map(|message| message.id.clone()).collect();
    if !json {
        let mut stdout = io::stdout().lock();
        for message in &messages {
            let to = match &message.recipient {
                Recipient::Agent { id, .. } => id,
                Recipient::User => "user",
            };
            writeln!(
                stdout,
                "Sent {} · thread {} · to {to}",
                message.id.0, message.thread.0
            )?;
        }
        stdout.flush()?;
    }
    if !wait {
        if json {
            super::print_json(json!({"messages": json_messages(messages)}))?;
        }
        return Ok(());
    }
    let reply = talk::wait(
        caller,
        threads.clone(),
        reply_to.clone(),
        timeout.unwrap_or(DEFAULT_TIMEOUT),
    )?;
    if json {
        let timed_out = reply.is_none();
        super::print_json(json!({
            "messages": json_messages(messages),
            "reply": reply.map(talk::json),
            "timed_out": timed_out,
            "threads": names(&threads),
            "reply_to": message_names(&reply_to),
            "resume": timed_out.then(|| resume(&threads, &reply_to)),
        }))?;
        if timed_out {
            std::process::exit(3);
        }
        Ok(())
    } else {
        wait_output(&threads, &reply_to, reply, false)
    }
}

fn names(threads: &[ThreadId]) -> Vec<&str> {
    threads.iter().map(|id| id.0.as_str()).collect()
}

fn message_names(messages: &[MessageId]) -> Vec<&str> {
    messages.iter().map(|id| id.0.as_str()).collect()
}

/// The command that resumes a timed-out wait with the same reply filter.
fn resume(threads: &[ThreadId], reply_to: &[MessageId]) -> String {
    let mut command = format!("power talk wait {}", names(threads).join(" "));
    for id in reply_to {
        command.push_str(" --reply-to ");
        command.push_str(&id.0);
    }
    command
}

fn wait_output(
    threads: &[ThreadId],
    reply_to: &[MessageId],
    reply: Option<Message>,
    json: bool,
) -> Result<()> {
    let timed_out = reply.is_none();
    if json {
        super::print_json(json!({
            "reply": reply.map(talk::json),
            "timed_out": timed_out,
            "threads": names(threads),
            "reply_to": message_names(reply_to),
            "resume": timed_out.then(|| resume(threads, reply_to)),
        }))?;
    } else if let Some(reply) = reply {
        println!("{}", envelope(&reply));
    } else {
        eprintln!(
            "Timed out waiting for a reply. Thread: {}. Resume with: {}",
            names(threads).join(", "),
            resume(threads, reply_to)
        );
    }
    if timed_out {
        std::process::exit(3);
    }
    Ok(())
}

fn json_messages(messages: Vec<Message>) -> Vec<serde_json::Value> {
    messages.into_iter().map(talk::json).collect()
}

fn print_messages(messages: Vec<Message>, json: bool, delivery: bool) -> Result<()> {
    if json {
        return super::print_json(json!({"messages": json_messages(messages)}));
    }
    if messages.is_empty() {
        println!("No messages.");
    }
    for message in &messages {
        println!("{}", envelope(message));
        if delivery {
            let state = &message.delivery;
            let times = [
                ("Queued", Some(state.queued_at)),
                ("Notified", state.notified_at),
                ("Reminded", state.reminded_at),
                ("Read", state.read_at),
            ];
            let line = times
                .into_iter()
                .filter_map(|(label, stamp)| {
                    let time =
                        chrono::DateTime::from_timestamp_millis(i64::try_from(stamp?).ok()?)?;
                    Some(format!("{label} {}", time.to_rfc3339()))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            println!("{line}");
        }
    }
    Ok(())
}
