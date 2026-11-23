//! What ends each command's help, below its options. [`NOTES`] lists the examples, the exit
//! statuses and a line or two more of each of `pumpkin`'s commands, and [`ADMIN`] those of
//! `pumpkin-admin`'s. [`section`] lays out each list as clap lays out its own, in clap's styles.
//! The module also holds the topics that `pumpkin help` prints.

use std::ffi::OsString;

use super::{Exit, HEADER, LITERAL, Program};

/// What ends the help of the command that `command` names, the words after the program's name,
/// or of the program itself, if there are none.
pub(super) struct Notes {
    pub(super) command: &'static [&'static str],
    examples: &'static [Example],
    statuses: &'static [Status],
    /// Prose that ends the help, or nothing.
    more: &'static str,
}

/// A command line that the help shows as a person types it in a shell. The program parses the
/// part that it gets.
pub(super) struct Example {
    /// What the shell runs before the program, such as `cd <dir> && `, or nothing.
    before: &'static str,
    /// The program's name and its arguments. A space or a line break comes between two arguments,
    /// and the help breaks the line at a line break. The line holds no syntax of the shell but a
    /// glob, which is checked as written.
    line: &'static str,
    /// What the shell does with the program's output, such as ` > outcome.json`, or nothing.
    after: &'static str,
    about: &'static str,
}

// An example that is the program's command line alone.
const ALONE: Example = Example {
    before: "",
    line: "",
    after: "",
    about: "",
};

// The characters of the shell's syntax that `line` holds none of, so that shell text can't slip
// into the part that the check parses.
const SHELL: [char; 9] = ['>', '<', '|', '&', ';', '$', '`', '\'', '"'];

/// An exit status of a command, with what it means for that command.
pub(super) struct Status {
    exit: Exit,
    about: &'static str,
}

// The statuses that both programs share, which each program's own help lists. A command's help
// lists the others.
const SUCCESS: Status = Status {
    exit: Exit::Success,
    about: "Success.",
};
const FAILURE: Status = Status {
    exit: Exit::Failure,
    about: "An error. A `pumpkin:` line on the standard error says why.",
};
const USAGE: Status = Status {
    exit: Exit::Usage,
    about: "A usage error.",
};

// One table for each program that the help is made from and that the check of its examples reads,
// as ripgrep's `FLAGS` is the one list that its parser and its help read:
// https://github.com/BurntSushi/ripgrep/blob/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/core/flags/defs.rs#L39-L44
// https://github.com/BurntSushi/ripgrep/blob/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/core/flags/doc/help.rs#L111-L113
pub(super) const NOTES: &[Notes] = &[
    Notes {
        command: &[],
        examples: &[],
        statuses: &[
            SUCCESS,
            FAILURE,
            USAGE,
            Status {
                exit: Exit::Partial,
                about: "An outcome short of success, which the command's help says.",
            },
        ],
        more: "`pumpkin help <command>` prints a command's help, and `pumpkin help <topic>`\n\
               prints a topic. The guide to running workflows is docs/running.md:\n\
               https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md",
    },
    Notes {
        command: &["check"],
        examples: &[
            Example {
                line: "pumpkin check tasks/bench/task.star --arg model=model-a",
                about: "Check a task with its arg `model` set to `model-a`",
                ..ALONE
            },
            Example {
                line: "pumpkin check tasks/*/task.star --arg model=model-a",
                about: "Check every task in `tasks`, each with the same arg",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "The lint's rules:\n\
               https://github.com/FrontierCS/Pumpkin/blob/main/docs/language.md#formatting-and-the-lint",
    },
    Notes {
        command: &["fmt"],
        examples: &[
            Example {
                line: "pumpkin fmt",
                about: "Format every `.star` file below the working directory",
                ..ALONE
            },
            Example {
                line: "pumpkin fmt --check lib tasks/bench/task.star",
                about: "Check that `lib` and a task's entry are formatted, and change nothing",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "",
    },
    Notes {
        command: &["run"],
        examples: &[
            Example {
                line: "pumpkin run tasks/bench/task.star --arg model=model-a",
                about: "Start a run of a task, and print its id",
                ..ALONE
            },
            Example {
                line: "pumpkin run tasks/bench/task.star --arg model=model-a --wait --outcome",
                after: "\n> outcome.json",
                about: "Start a run, follow it to its end, and save its outcome.json",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "How a run is placed and how it outlives a logout:\n\
               https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md#running",
    },
    Notes {
        command: &["wait"],
        examples: &[
            Example {
                line: "pumpkin wait 20261108-000905-126a21e9",
                about: "Wait for the run with this id",
                ..ALONE
            },
            Example {
                line: "pumpkin wait 126a",
                about: "Wait for the one run whose id's last 8 digits start with `126a`",
                ..ALONE
            },
            Example {
                line: "pumpkin wait runs/20261108-000905-126a21e9",
                about: "Wait for the run that this record holds",
                ..ALONE
            },
            Example {
                line: "pumpkin wait --outcome 126a",
                after: " > outcome.json",
                about: "Wait for a run, and save its outcome.json once it ends",
                ..ALONE
            },
        ],
        statuses: &[
            Status {
                exit: Exit::Success,
                about: "The run finished with a result.",
            },
            Status {
                exit: Exit::Failure,
                about: "The run failed, was interrupted or was lost, or `wait` failed.",
            },
            Status {
                exit: Exit::Partial,
                about: "The run finished without a result.",
            },
        ],
        more: "",
    },
    Notes {
        command: &["ls"],
        examples: &[
            Example {
                line: "pumpkin ls",
                about: "List the runs that are live or ended within the last day",
                ..ALONE
            },
            Example {
                line: "pumpkin ls 20261110-1",
                about: "List the runs whose ids start with `20261110-1`",
                ..ALONE
            },
            Example {
                line: "pumpkin ls --runs runs",
                about: "List the runs recorded in `runs`",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "The phases and states:\n\
               https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md#following-and-listing-runs",
    },
    Notes {
        command: &["cancel"],
        examples: &[Example {
            line: "pumpkin cancel 126a",
            about: "Stop the one run whose id's last 8 digits start with `126a`",
            ..ALONE
        }],
        statuses: &[
            Status {
                exit: Exit::Success,
                about: "The run has ended, and nothing that it left remains.",
            },
            Status {
                exit: Exit::Partial,
                about: "The run has ended, but one of its providers can't be reached. What the \
                        run left there may remain.",
            },
        ],
        more: "",
    },
    Notes {
        command: &["view"],
        examples: &[
            Example {
                line: "pumpkin view",
                about: "Serve the runs in the repository's `.pumpkin`",
                ..ALONE
            },
            Example {
                line: "pumpkin view --runs runs --port 8080",
                about: "Serve the runs recorded in `runs` at port 8080",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "`view` serves until a signal stops it, so it never exits 0.\n\n\
               What the page shows:\n\
               https://github.com/FrontierCS/Pumpkin/blob/main/docs/record.md#the-page",
    },
    Notes {
        command: &["help"],
        examples: &[
            Example {
                line: "pumpkin help run",
                about: "Print the help of `run`",
                ..ALONE
            },
            Example {
                line: "pumpkin help config",
                about: "Print the topic `config`, which describes pumpkin.toml",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "",
    },
    Notes {
        command: &["reap"],
        examples: &[
            Example {
                line: "pumpkin reap",
                about: "Remove what the ended runs in the repository's `.pumpkin` left",
                ..ALONE
            },
            Example {
                line: "pumpkin reap --runs runs",
                about: "Remove what the ended runs recorded in `runs` left",
                ..ALONE
            },
        ],
        statuses: &[
            Status {
                exit: Exit::Success,
                about: "Nothing that the runs left remains.",
            },
            Status {
                exit: Exit::Partial,
                about: "`reap` can't reach a provider of a run, which it names, and nothing else \
                        failed. What the run left there may remain. The provider's Docker context \
                        may be gone, or the configuration may no longer name the provider.",
            },
        ],
        more: "",
    },
    Notes {
        command: &["controller"],
        examples: &[
            Example {
                before: "cd ~/trees/tasks/20261108-000905-126a21e9 &&\n",
                line: "pumpkin controller job 20261108-000905-126a21e9\n\
                       --tree 5d0c5f0e83a3c40be3e7f2b4d1f9a2b6c8e7d6a5b4c3d2e1f0a9b8c7d6e5f4a3\n\
                       -- run tasks/bench/task.star --arg model=model-a",
                about: "Run a job in its tree, as its unit does",
                ..ALONE
            },
            Example {
                line: "pumpkin controller boot",
                about: "Reap what lost runs left, as the controller's boot does",
                ..ALONE
            },
        ],
        statuses: &[],
        more: "",
    },
];

const INSTALL: Example = Example {
    before: "sudo ./pumpkin/bin/",
    line: "pumpkin-admin install --settings\n\
           arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings",
    after: "",
    about: "Install this release on the controller host, as root",
};
const CHECK: Example = Example {
    line: "pumpkin-admin check --settings\n\
           arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings",
    about: "Check the deployment whose settings the ARN names",
    ..ALONE
};

pub(super) const ADMIN: &[Notes] = &[
    Notes {
        command: &[],
        examples: &[
            INSTALL,
            CHECK,
            Example {
                line: "pumpkin-admin policy controller",
                about: "Print the IAM policy of the controller's role",
                ..ALONE
            },
        ],
        statuses: &[SUCCESS, FAILURE, USAGE],
        more: "`pumpkin-admin <command> --help` prints a command's help.",
    },
    Notes {
        command: &["install"],
        examples: &[INSTALL],
        statuses: &[],
        more: "The settings name the members' list in their `[deployment] members`. The list\n\
               holds a line `<member> <key>` for each key. A member's name is 1 to 32 of a-z,\n\
               0-9 and `-`, and starts with a letter. The key is the line of the member's\n\
               ~/.config/pumpkin/ssh/key.pub. A list with any other line, or a key twice, is\n\
               refused whole.",
    },
    Notes {
        command: &["check"],
        examples: &[CHECK],
        statuses: &[
            Status {
                exit: Exit::Success,
                about: "`check` found no problem.",
            },
            Status {
                exit: Exit::Failure,
                about: "`check` found a problem, or failed.",
            },
        ],
        more: "",
    },
    Notes {
        command: &["check-members"],
        examples: &[Example {
            line: "pumpkin-admin check-members members",
            about: "Check the key files of the members in the directory `members`",
            ..ALONE
        }],
        statuses: &[],
        more: "",
    },
];

impl Notes {
    /// The text that ends the command's help, its sections apart by a blank line.
    pub(super) fn render(&self) -> String {
        let Self {
            command: _,
            examples,
            statuses,
            more,
        } = self;
        let typed: Vec<String> = (examples.iter())
            .map(|example| {
                let Example {
                    before,
                    line,
                    after,
                    about: _,
                } = example;
                format!("{before}{line}{after}")
            })
            .collect();
        let examples: Vec<(&str, &str)> = (typed.iter().zip(*examples))
            .map(|(typed, example)| (typed.as_str(), example.about))
            .collect();
        let sections = [
            section("Examples", &examples, Layout::NextLine),
            statuses_section(statuses),
            (*more).to_owned(),
        ];
        let sections: Vec<String> = (sections.into_iter())
            .filter(|section| !section.is_empty())
            .collect();
        sections.join("\n\n")
    }
}

/// The section of `statuses`, or nothing without one.
fn statuses_section(statuses: &[Status]) -> String {
    let codes: Vec<String> = (statuses.iter())
        .map(|Status { exit, about: _ }| exit.code().to_string())
        .collect();
    let rows: Vec<(&str, &str)> = (codes.iter().zip(statuses))
        .map(|(code, Status { exit: _, about })| (code.as_str(), *about))
        .collect();
    section("Exit status", &rows, Layout::Columns)
}

/// Asserts that each program's `parse` takes each of its examples' lines, and that a help lists
/// each exit status.
pub(super) fn check() {
    for program in Program::ALL {
        for notes in program.notes() {
            for example in notes.examples {
                let refused = refused(program, notes, example);
                assert!(
                    refused.is_none(),
                    "the example `{}` is refused: {}",
                    example.line,
                    refused.unwrap_or_default()
                );
            }
        }
    }
    for exit in Exit::ALL {
        let mut listed = (Program::ALL.into_iter())
            .flat_map(Program::notes)
            .flat_map(|notes| notes.statuses);
        assert!(
            listed.any(|status| status.exit == *exit),
            "no help lists the exit status {}",
            exit.code()
        );
    }
}

/// Why `example`, of the entry `notes` of `program`, can't be shown, or nothing if it can.
fn refused(program: Program, notes: &Notes, example: &Example) -> Option<String> {
    let Example {
        before,
        line,
        after,
        about,
    } = example;
    let prefix = format!("{} ", program.name());
    let Some(args) = line.strip_prefix(&prefix) else {
        return Some(format!("it doesn't start with `{prefix}`"));
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let around = [before, after].map(|text| text.split_whitespace());
    if line.contains(SHELL) {
        Some("it holds the shell's syntax".to_owned())
    } else if !args.starts_with(notes.command) {
        Some("it isn't a line of the command that its entry names".to_owned())
    } else if around
        .into_iter()
        .flatten()
        .any(|word| word.starts_with('-'))
    {
        Some("its `before` or `after` holds an option, which nothing parses".to_owned())
    } else if about.is_empty() {
        Some("it has no description".to_owned())
    } else {
        (program.parse(args.into_iter().map(OsString::from)).err())
            .map(|refusal| refusal.error.render().to_string())
    }
}

/// How a section lays out its rows. `Columns` lays them out as clap lays out its commands. Each
/// row's text is in a column after the longest literal. `NextLine` lays them out as clap's long
/// help lays out its arguments. Each row's text is on the line below its literal, and a blank line
/// comes between two rows.
#[derive(Clone, Copy)]
pub(super) enum Layout {
    Columns,
    NextLine,
}

// clap's indent of a row, and its further indent of a long help's text below its argument.
// https://github.com/clap-rs/clap/blob/13f2db5072d600c11d8d6298e4e9ba53ffc6c1ab/clap_builder/src/output/mod.rs#L21
// https://github.com/clap-rs/clap/blob/13f2db5072d600c11d8d6298e4e9ba53ffc6c1ab/clap_builder/src/output/help_template.rs#L1088
const TAB: &str = "  ";
const NEXT_LINE_INDENT: &str = "        ";

// clap wraps the text after a help again, at the terminal's width, and a line that it wraps loses
// its indent. So each line fits 80 columns, a terminal's common width.
// https://github.com/clap-rs/clap/blob/13f2db5072d600c11d8d6298e4e9ba53ffc6c1ab/clap_builder/src/output/help_template.rs#L360-L365
const WIDTH: usize = 80;

// A row's literal has the literal style, as rustup styles its exit statuses:
// https://github.com/rust-lang/rustup/blob/08f60b0a9f445bf25153692139700a2cbc1a869b/src/cli/help.rs#L219-L226
/// The `rows` under `heading`, each a literal, such as a command, and what it means, or nothing
/// without a row. Each row's text is wrapped to 80 columns, at the indent of its first line.
pub(super) fn section(heading: &str, rows: &[(&str, &str)], layout: Layout) -> String {
    let Some(longest) = rows.iter().map(|(literal, _)| width(literal)).max() else {
        return String::new();
    };
    let rows: Vec<String> = (rows.iter())
        .map(|(literal, about)| match layout {
            Layout::Columns => {
                // The padding is unstyled, as in clap's list.
                let pad = " ".repeat(longest - width(literal));
                let indent = " ".repeat(TAB.len() + longest + TAB.len());
                let about = wrapped(about, WIDTH - indent.len()).join(&format!("\n{indent}"));
                format!("{TAB}{LITERAL}{literal}{LITERAL:#}{pad}{TAB}{about}")
            }
            Layout::NextLine => {
                let lines: Vec<String> = (literal.lines())
                    .map(|line| {
                        let room = WIDTH - TAB.len() * 2 - " \\".len();
                        assert!(
                            width(line) <= room,
                            "the line `{line}` is over {room} columns"
                        );
                        format!("{LITERAL}{line}{LITERAL:#}")
                    })
                    .collect();
                // A line that goes on below ends in ` \`, as in a shell, and the next is indented
                // further.
                let literal = lines.join(&format!(" \\\n{TAB}{TAB}"));
                let indent = format!("{TAB}{NEXT_LINE_INDENT}");
                let about = wrapped(about, WIDTH - indent.len()).join(&format!("\n{indent}"));
                format!("{TAB}{literal}\n{indent}{about}")
            }
        })
        .collect();
    let between = match layout {
        Layout::Columns => "\n",
        Layout::NextLine => "\n\n",
    };
    format!("{HEADER}{heading}:{HEADER:#}\n{}", rows.join(between))
}

/// The words of `text` in lines of at most `width` columns, each longer word on a line of its own.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if self::width(line) + 1 + self::width(word) <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

fn width(text: &str) -> usize {
    text.chars().count()
}

pub(super) const CONFIG: &str = "\
pumpkin.toml is the machine's configuration, which `pumpkin run` reads, and
`pumpkin check` too, for the model routes. It is
the file that --config names, by default $HOME/.config/pumpkin/pumpkin.toml. It
is TOML, with the tables below. Every key is required unless it says what its
absence means. [gateway] and the [upstreams.<name>] tables may be left out. Any
other key is refused. On a machine whose runs go to the person's controller,
[controller] takes the place of [docker], [providers.<name>.<kind>] and
[gateway].

Example, of a machine whose runs go to EC2 and Modal:
  [docker]
  context = \"default\"

  [providers.aws.ec2]
  region = \"us-west-1\"
  repositories = { roles = \"pumpkin/roles\", sandboxd = \"pumpkin/sandboxd\" }

  [providers.aws.ec2.timer]
  group = \"pumpkin\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-timer\"

  [providers.aws.ec2.classes.cpu]
  template = \"lt-0123456789abcdef0\"
  version = 1
  vcpus = 8
  memory_mib = 16384
  cpu_features = [\"avx2\"]

  [providers.gpus.modal]
  profile = \"pumpkin\"
  dials_from = \"203.0.113.7\"

  [providers.gpus.modal.images]
  region = \"us-west-1\"
  repository = \"pumpkin/roles\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-modal-pull\"

  [providers.gpus.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

  [gateway]
  private = \"172.31.0.10:7433\"

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"

  [[upstreams.gateway.models]]
  name = \"model-a\"
  upstream_name = \"provider/model-a\"
  usd_per_mtok = { input = 1.25, cached = 0.125, output = 10 }

Example, of a machine whose runs go to the person's controller, and Modal:
  [controller]
  name = \"alice\"
  region = \"us-west-1\"

  [controller.modal]
  profile = \"pumpkin\"

  [controller.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

[docker] is the machine's Docker engine:
  context       The context that reaches it, as `docker context ls` lists it.

[providers.<name>.<kind>] is a provider that the machine's runs may place roles
on. <name> names it, and <kind> is its kind. Its table holds the kind's
settings:
  docker        Sandboxes on the engine of [docker]. This kind takes no
                settings. A docker provider must be the configuration's only
                provider: a machine runs its roles either on its own engine or
                on providers of kind ec2 and modal. Its one class,
                `container`, offers the machine's online CPUs and its
                memory. It offers the CPU flags that /proc/cpuinfo
                lists for every CPU, no CPU model and no GPU.
  ec2           Sandboxes on EC2 instances of their own, in the envelope that
                its settings name. The instances pull their images from the
                envelope, which the engine of [docker] pushes them to. They
                dial the private address of [gateway]. The provider runs AWS
                CLI v2 as `aws` from PATH. It uses the credentials that `aws`
                finds, such as the instance's role. Its settings:
    region        The envelope's AWS region, which every call names.
    classes       The classes of sandbox, by name. There is at least one. Each
                  class has these keys:
                  `template` is the ID of its launch template, and `version`
                  the template's version that its instances launch from.
                  `vcpus` and `memory_mib` are its instance type's CPUs and
                  memory in MiB. A role gets all the CPUs, and all the memory
                  but a share that the instance keeps.
                  `cpu_features` are its CPU's flags, and `cpu_model` is its
                  CPU's model, as /proc/cpuinfo names them. Without
                  `cpu_features`, the class states no flag. Without
                  `cpu_model`, it states no model. A class that states a model
                  is offered only to roles that need it.
                  `usd_per_hour` is what an instance costs an hour. With it,
                  `ls` and a run's summary price the class's sandboxes.
                  `startup_seconds` is the most seconds that an instance's
                  start may take, its images' pulls included. Without it, the
                  start may take 1740 seconds.
    repositories  The envelope's ECR repositories: `roles` holds the roles'
                  images, and `sandboxd` the images that hold sandboxd.
    timer         The sandboxes' timer, which terminates each instance at its
                  run's hard end. `group` is its EventBridge Scheduler schedule
                  group. `role_arn` is the ARN of the role that its schedules
                  terminate instances with.
  modal         Sandboxes in a Modal workspace. Modal imports their images from
                an ECR repository, which the engine of [docker] pushes them to.
                Each sandbox listens behind Modal's TLS tunnel, which the run
                dials. The provider runs Modal's helper, pumpkin-modal.py,
                beside `pumpkin`, with `uv` from PATH. It runs `aws`, AWS CLI
                v2, as ec2 does. Its settings:
    profile       The profile of Modal's configuration whose token reaches the
                  workspace.
    config        The absolute path of Modal's configuration file that holds
                  the profile. Without it, the file is ~/.modal.toml.
    dials_from    This machine's public IPv4 address. The sandboxes' tunnels
                  take connections from it alone.
    images        Where Modal imports images from. `region` is an AWS region,
                  and `repository` an ECR repository there. `role_arn` is the
                  ARN of the AWS role that Modal pulls with, which trusts
                  Modal's identity.
    classes       The classes of sandbox, by name. There is at least one. Each
                  class has `cpu_millis`, `memory_bytes` in whole MiB and
                  `startup_seconds`. A class with a GPU has `gpu`, as Modal
                  names one, such as \"H100!\", or \"A100-80GB:2\" for two. It
                  then has `gpu_model`, the model as nvidia-smi names it, such
                  as \"NVIDIA H100 80GB HBM3\". A class with a GPU is offered
                  only to roles that need its GPUs. `usd_per_hour` is what a
                  sandbox costs an hour. With it, `ls` and a run's summary
                  price the class's sandboxes.

[gateway] is where the gateway meets the sandboxes that dial it over a network:
  private       The address, ip:port, that the sandboxes of an ec2 provider
                dial. It is one of this machine's addresses on the envelope's
                network, at a port that the envelope's security group admits.
                A run listens there while it places a role on such a provider,
                so only one such run can go at a time.

[upstreams.<name>] is an upstream that a workflow's model route may reach:
  url           The base URL of its API. It is https, or http to a loopback IP
                address, such as 127.0.0.1 but not localhost. The route
                reaches each API's endpoint below it: /responses,
                /chat/completions, or /messages, which an Anthropic Messages
                API serves, such as https://api.anthropic.com/v1.
  key_file      The file that holds its key, below this file's directory unless
                the path is absolute. The route sends the key as
                `authorization: Bearer <key>`, and to /messages as
                `x-api-key: <key>`.
  models        The models it serves. Each has the `name` that a route allows
                and the `upstream_name` that the upstream takes for it.
                `usd_per_mtok` is optional. It says what the model's tokens
                cost in US dollars a million: `input` and `output`, and
                `cached` and `cache_write`, the input that the upstream read
                from its cache and wrote to it. Reasoning counts as output. A
                model without `usd_per_mtok` has no cost. Neither has a model
                whose responses report a kind of token that its prices lack.
                No model is listed twice, under one upstream or two.

Each entry of a workflow's model route reaches the one upstream that serves
every model it allows.

On a controller, each job reads its configuration from the controller's
settings, which deploy/aws writes. The job sets [gateway] itself. The client
sends [upstreams] and [controller.modal]. The settings name `envelope`, the
version of the envelope that wrote them. deploy/aws's README says which
versions a job takes. The admin sets `draining = true` for a cutover. Each job
is then refused as it starts, and the jobs that run go on to their ends.

[controller] is the person's controller, which deploy/aws makes. `run`, `wait`,
`ls`, `cancel`, `view` and `reap` go to it, as each one's help says.
Each run is sent the [upstreams.<name>] tables, their keys, [controller.modal]
and Modal's configuration file. They go into a directory of the run's own in
the controller's tmpfs, which the run's end removes. The job runs the client's
install with the module's settings and [controller.modal].
The client uses AWS CLI v2, ssh, ssh-keygen and git from PATH. It sends the
task's files and starts its job over SSH to the controller's private address.
It uses up to four connections for large task uploads. The machine is in the
controller's VPC, or reaches it through a VPN that translates its addresses
into the VPC's. The first command creates a key. The controller admits the key
once its admin lists it under the person's name in deploy/aws's `members`.
Until then, each command is refused with the line to send the admin. The client
keeps that key and the pinned host key in $HOME/.config/pumpkin/ssh/.
  name          The `name` that deploy/aws was applied with.
  region        The region it was applied in.

[controller.modal] is the Modal provider that the controller's runs may place
roles on, if any. It takes the settings of a provider of kind modal, but
`images`, which the module's settings give, and `dials_from`, which each run
sets:
  profile       As modal's. The profile is in Modal's configuration file,
                ~/.modal.toml. Each run is sent that file if it is there.
                Its workspace is one that deploy/aws's `modal_workspaces`
                lists.
  classes       As modal's.
";
