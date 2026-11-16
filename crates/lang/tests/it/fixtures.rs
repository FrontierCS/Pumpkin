//! The files of `tests/fixtures`, listed rather than named, so that none goes unchecked: each
//! folder says what its entries, the files named `task.star`, must do, as `Folder`'s variants
//! say, and each file but `bad/stack/lib.star`, cut at every eighth character and at each line's
//! end, must evaluate or give problems that read, as `Case::prefixes` has it.

use std::collections::BTreeMap;

use expect_test::{Expect, expect};
use lang::{Args, Bounds};

use crate::{Case, files, on_stack};

/// A folder at the top of `tests/fixtures`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Folder {
    /// Whole tasks, whose entries must evaluate.
    Tasks,
    /// Files too long to read in a case, whose entries must give problems.
    Bad,
    /// Libraries, which cases and entries load, and which hold no entry.
    Lib,
}

impl Folder {
    fn of(path: &str) -> Self {
        match path.split_once('/') {
            Some(("tasks", _)) => Self::Tasks,
            Some(("bad", _)) => Self::Bad,
            Some(("lib", _)) => Self::Lib,
            _ => panic!(
                "`{path}` is in no folder of the fixtures: move it to `tasks/`, `bad/` or `lib/`"
            ),
        }
    }
}

fn check(folder: Folder, expect: &Expect) {
    let files = files();
    let shown = on_stack(|| {
        let args = Args::parse([]).unwrap();
        let shown: Vec<String> = (files.keys())
            .filter(|path| Folder::of(path) == folder)
            .map(|path| show(folder, path, &files, &args))
            .collect();
        shown.join("\n\n")
    });
    expect.assert_eq(&shown);
}

/// What the file at `path`, in `folder`, shows, under its path: how many of its prefixes
/// evaluate, as the entry `text.star` loads `X` from it unless it is an entry itself; and then,
/// if it is, its evaluation, as `Case::evaluate` gives it.
fn show(folder: Folder, path: &str, files: &BTreeMap<String, String>, args: &Args) -> String {
    let is_entry = path.ends_with("/task.star");
    let loads = format!("load(\"//{path}\", \"X\")");
    let case = Case {
        entry: if is_entry { path } else { "text.star" },
        held: if is_entry {
            Vec::new()
        } else {
            vec![("text.star", loads.as_str())]
        },
        files,
        bounds: Bounds::WORKFLOW,
    };
    // Its prefixes cut what the other fixtures' prefixes cut, only deeper, and would take most
    // of the tests' time.
    let swept = (path != "bad/stack/lib.star").then(|| {
        // Every eighth character, as a cut at each took 5 s of the tests' time.
        let (evaluated, cuts) = case.prefixes(path, args, 8);
        format!(": {evaluated} of {cuts} prefixes evaluate")
    });
    let evaluated = is_entry.then(|| case.evaluate(args));
    match (folder, &evaluated) {
        (Folder::Tasks, Some(Err(problems))) => {
            panic!("`{path}` gives problems, and an entry in `tasks/` must evaluate:\n{problems}")
        }
        (Folder::Bad, Some(Ok(_))) => {
            panic!("`{path}` evaluates, and an entry in `bad/` must give problems")
        }
        (Folder::Lib, Some(_)) => {
            panic!(
                "`{path}` is an entry, and `lib/` holds libraries: move it to `tasks/` or `bad/`"
            )
        }
        (Folder::Tasks, Some(Ok(_)) | None)
        | (Folder::Bad, Some(Err(_)) | None)
        | (Folder::Lib, None) => {}
    }
    let evaluated = evaluated.map(|(Ok(shown) | Err(shown))| format!("\n{shown}"));
    let (swept, evaluated) = (swept.unwrap_or_default(), evaluated.unwrap_or_default());
    format!("== {path}{swept}{evaluated}")
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn tasks() {
    check(
        Folder::Tasks,
        &expect![[r#"
            == tasks/judged/task.star: 2 of 65 prefixes evaluate
            {
              "ir_version": "pumpkin.ir/v0",
              "name": "neutral/judged-neutral",
              "roles": [
                {
                  "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                  "name": "agent",
                  "needs": {
                    "cpu_millis": 4000,
                    "memory_bytes": 8589934592
                  },
                  "runtime": {
                    "agent": {
                      "command": {
                        "argv": [
                          "/usr/local/bin/agent"
                        ],
                        "cwd": "/workspace"
                      },
                      "deadline_seconds": 18000,
                      "egress": {
                        "model": [
                          {
                            "api": "responses",
                            "models": [
                              "model-large",
                              "model-small"
                            ]
                          }
                        ]
                      },
                      "finalize_grace_seconds": 30,
                      "grants": [
                        {
                          "judge": {
                            "call_timeout_seconds": 600,
                            "calls": 20,
                            "role": "train_judge",
                            "slot": "payload"
                          },
                          "name": "train"
                        },
                        {
                          "name": "final",
                          "submit": {}
                        }
                      ],
                      "stdin": "Solve judged_neutral, scored by 3 metrics: solved, _internal, rejected.\n"
                    }
                  },
                  "user": {
                    "gid": 1000,
                    "uid": 1000
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "train_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 1073741824
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run",
                          "--mode",
                          "train"
                        ],
                        "cwd": "/pumpkin/home",
                        "env": [
                          "LEVEL=hard",
                          "GRACE=30",
                          "MODE=train",
                          "SLOT_BYTES=65536"
                        ]
                      },
                      "result": {
                        "returns": [
                          "/score",
                          "/metrics/solved",
                          "/metrics/rejected",
                          "/failure"
                        ],
                        "shows": {
                          "score": "/score"
                        }
                      },
                      "slots": [
                        {
                          "max_bytes": 65536,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "final_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 1073741824
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run",
                          "--mode",
                          "eval"
                        ],
                        "cwd": "/pumpkin/home",
                        "env": [
                          "LEVEL=hard",
                          "GRACE=30",
                          "MODE=eval",
                          "SLOT_BYTES=1048576"
                        ]
                      },
                      "result": {
                        "returns": [
                          "/score",
                          "/failure"
                        ],
                        "shows": {
                          "score": "/score"
                        }
                      },
                      "slots": [
                        {
                          "max_bytes": 1048576,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ],
              "score": {
                "role": "final_judge",
                "slot": "payload",
                "timeout_seconds": 120
              }
            }

            == tasks/literal/task.star: 2 of 569 prefixes evaluate
            {
              "ir_version": "pumpkin.ir/v0",
              "name": "neutral/literal",
              "roles": [
                {
                  "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                  "name": "agent",
                  "needs": {
                    "cpu_features": [
                      "avx2",
                      "avx512f"
                    ],
                    "cpu_millis": 4000,
                    "gpu": {
                      "count": 1,
                      "model": "NVIDIA H100 80GB HBM3"
                    },
                    "memory_bytes": 17179869184
                  },
                  "runtime": {
                    "agent": {
                      "command": {
                        "argv": [
                          "/usr/local/bin/agent",
                          "--quiet"
                        ],
                        "cwd": "/workspace"
                      },
                      "deadline_seconds": 18000,
                      "dirs": [
                        {
                          "mode": "0700",
                          "path": "/workspace/notes"
                        }
                      ],
                      "egress": {
                        "model": [
                          {
                            "api": "responses",
                            "models": [
                              "model-large",
                              "model-small"
                            ]
                          }
                        ]
                      },
                      "files": [
                        {
                          "content": "tab\there, quote ' and AAé😀, \u0007\b\f\r\u000b, joined\n",
                          "mode": "0644",
                          "path": "/workspace/notes/escapes.txt"
                        }
                      ],
                      "finalize_grace_seconds": 60,
                      "grants": [
                        {
                          "judge": {
                            "call_timeout_seconds": 600,
                            "calls": 100,
                            "role": "train_judge",
                            "slot": "payload"
                          },
                          "name": "train"
                        },
                        {
                          "name": "final",
                          "submit": {}
                        }
                      ],
                      "stdin": "Solve the task in /task/README.md, then submit with \"sandboxctl\".\n"
                    }
                  },
                  "user": {
                    "gid": 1000,
                    "uid": 1000
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "train_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run"
                        ],
                        "cwd": "/pumpkin/home",
                        "env": [
                          "MODE=train",
                          "PATTERN=\\d+\\.json"
                        ]
                      },
                      "result": {
                        "returns": [
                          "/score",
                          "/metrics",
                          "/failure"
                        ],
                        "shows": {
                          "score": "/score"
                        }
                      },
                      "slots": [
                        {
                          "max_bytes": 16384,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "final_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run"
                        ],
                        "cwd": "/pumpkin/home",
                        "env": [
                          "MODE=eval"
                        ]
                      },
                      "result": {
                        "returns": [
                          "/score"
                        ],
                        "shows": {
                          "score": "/score"
                        }
                      },
                      "slots": [
                        {
                          "max_bytes": 16384,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ],
              "score": {
                "role": "final_judge",
                "slot": "payload",
                "timeout_seconds": 120
              }
            }

            == tasks/loaded/task.star: 2 of 115 prefixes evaluate
            {
              "ir_version": "pumpkin.ir/v0",
              "name": "neutral/loaded",
              "roles": [
                {
                  "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                  "name": "agent",
                  "needs": {
                    "cpu_millis": 2000,
                    "memory_bytes": 4294967296
                  },
                  "runtime": {
                    "agent": {
                      "command": {
                        "argv": [
                          "/usr/local/bin/agent"
                        ],
                        "cwd": "/workspace"
                      },
                      "deadline_seconds": 3600,
                      "finalize_grace_seconds": 0,
                      "grants": [
                        {
                          "name": "final",
                          "submit": {}
                        }
                      ],
                      "stdin": "Solve the task in /task/README.md.\n"
                    }
                  },
                  "user": {
                    "gid": 1000,
                    "uid": 1000
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "final_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run"
                        ],
                        "cwd": "/pumpkin/home"
                      },
                      "result": {
                        "returns": [
                          "/score",
                          "/failure"
                        ],
                        "shows": {
                          "score": "/score"
                        }
                      },
                      "slots": [
                        {
                          "max_bytes": 65536,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ],
              "score": {
                "role": "final_judge",
                "slot": "payload",
                "timeout_seconds": 120
              }
            }"#]],
    );
}

#[test]
fn bad() {
    check(
        Folder::Bad,
        &expect![[r#"
            == bad/calls/lib.star: 0 of 350 prefixes evaluate

            == bad/calls/task.star: 0 of 7 prefixes evaluate
            bad/calls/lib.star:146:12: this call would run 49 functions at once: nest calls at most 48 deep
                146 |     return f48(x)
                    |            ^^^^^^
                in `f47()`, called at bad/calls/lib.star:143:12
                in `f46()`, called at bad/calls/lib.star:140:12
                in `f45()`, called at bad/calls/lib.star:137:12
                in `f44()`, called at bad/calls/lib.star:134:12
                in `f43()`, called at bad/calls/lib.star:131:12
                in `f42()`, called at bad/calls/lib.star:128:12
                in `f41()`, called at bad/calls/lib.star:125:12
                in `f40()`, called at bad/calls/lib.star:122:12
                in `f39()`, called at bad/calls/lib.star:119:12
                in `f38()`, called at bad/calls/lib.star:116:12
                in `f37()`, called at bad/calls/lib.star:113:12
                in `f36()`, called at bad/calls/lib.star:110:12
                in `f35()`, called at bad/calls/lib.star:107:12
                in `f34()`, called at bad/calls/lib.star:104:12
                in `f33()`, called at bad/calls/lib.star:101:12
                in `f32()`, called at bad/calls/lib.star:98:12
                in `f31()`, called at bad/calls/lib.star:95:12
                in `f30()`, called at bad/calls/lib.star:92:12
                in `f29()`, called at bad/calls/lib.star:89:12
                in `f28()`, called at bad/calls/lib.star:86:12
                in `f27()`, called at bad/calls/lib.star:83:12
                in `f26()`, called at bad/calls/lib.star:80:12
                in `f25()`, called at bad/calls/lib.star:77:12
                in `f24()`, called at bad/calls/lib.star:74:12
                in `f23()`, called at bad/calls/lib.star:71:12
                in `f22()`, called at bad/calls/lib.star:68:12
                in `f21()`, called at bad/calls/lib.star:65:12
                in `f20()`, called at bad/calls/lib.star:62:12
                in `f19()`, called at bad/calls/lib.star:59:12
                in `f18()`, called at bad/calls/lib.star:56:12
                in `f17()`, called at bad/calls/lib.star:53:12
                in `f16()`, called at bad/calls/lib.star:50:12
                in `f15()`, called at bad/calls/lib.star:47:12
                in `f14()`, called at bad/calls/lib.star:44:12
                in `f13()`, called at bad/calls/lib.star:41:12
                in `f12()`, called at bad/calls/lib.star:38:12
                in `f11()`, called at bad/calls/lib.star:35:12
                in `f10()`, called at bad/calls/lib.star:32:12
                in `f9()`, called at bad/calls/lib.star:29:12
                in `f8()`, called at bad/calls/lib.star:26:12
                in `f7()`, called at bad/calls/lib.star:23:12
                in `f6()`, called at bad/calls/lib.star:20:12
                in `f5()`, called at bad/calls/lib.star:17:12
                in `f4()`, called at bad/calls/lib.star:14:12
                in `f3()`, called at bad/calls/lib.star:11:12
                in `f2()`, called at bad/calls/lib.star:8:12
                in `f1()`, called at bad/calls/lib.star:5:12
                in `f0()`, called at bad/calls/lib.star:153:12

            == bad/stack/lib.star

            == bad/stack/task.star: 0 of 7 prefixes evaluate
            bad/stack/lib.star:85:18: `+` can't take an int and a bool: give it two numbers, two strings, two lists or two tuples
                85 |         t(INNER) + (DEEP == DEEP)
                   |                  ^
                the left operand, `1`, is made at bad/stack/lib.star:20:12
                the right operand, `True`, is made at bad/stack/lib.star:85:21
                in `deepest()`, called at bad/stack/lib.star:153:11"#]],
    );
}

#[test]
fn lib() {
    check(
        Folder::Lib,
        &expect![[r#"
            == lib/deep.star: 0 of 58 prefixes evaluate

            == lib/images.star: 0 of 500 prefixes evaluate

            == lib/judged.star: 0 of 564 prefixes evaluate

            == lib/parts.star: 0 of 226 prefixes evaluate"#]],
    );
}
